#[cfg(test)]
mod tests;

mod consent;
mod context;
mod doctor;
mod launch;
mod provider;
mod provider_process;
mod query;
mod requests;
mod self_test;
mod settings;
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
const UNPUBLISHED_EVENT_PREFIX: &str = "unpublished-";
/// The state root's durability receipt. It exists only after some creator synced the
/// directory entry of the root and of every ancestor up to the filesystem root or the
/// user's home directory, so a root that lacks it is not assumed durable merely because
/// it exists: the creator that made it may have stopped before those syncs.
const STATE_ROOT_DURABLE_FILE: &str = "state-root.durable";
/// Upper bound on the directory entries the state-root ancestry walk makes durable.
const STATE_ROOT_ANCESTRY_SYNC_LIMIT: usize = 16;
// Written into a closed source session by the one reopen that won its turn-claim lock. It is
// the only file a reopen ever adds to the source; the source's tombstone, events, and
// requests stay byte-for-byte intact.
const REOPEN_MARKER_FILE: &str = "reopen.marker.json";
// Written into the NEW session by a reopen gate that fails after the session exists: the
// launch wrapper's pre-spawn ownership recheck or the post-launch holder check. It carries
// the gate name across the process boundary so the reopen response can still report it,
// and it is the durable evidence from which a later reopen reconciles a source marker
// that its parent never settled (`verify_reopen_source_is_closed`).
const REOPEN_REFUSAL_FILE: &str = "reopen.refusal.json";
// Written by the launch wrapper immediately after it spawns the provider process and before
// the session leaves its launch state: the provider's pid and, on Windows, its creation time
// and executable path. The provider process is the only process that can hold a resumed
// conversation (Windows does not end children with their parent, and the wrapper exits
// after the provider), so a reopen refused after this point releases the source's marker
// only once the process this record names is verified gone (`refused_launch_cleanup`).
const PROVIDER_PROCESS_FILE: &str = "provider-process.json";
static TURN_CLAIM_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) enum NativeCommand {
    Consent(Vec<String>),
    Settings(Vec<String>),
    Ask(AskRequest),
    SelfTest(self_test::Request),
    Tell(TellRequest),
    Reopen(ReopenRequest),
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
    ConsoleHost {
        directory: PathBuf,
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
    pub(crate) context_results: Vec<context::ContextResultRef>,
}

#[derive(Debug)]
pub(crate) struct TellRequest {
    id: String,
    prompt: String,
    timeout: Duration,
    detach: bool,
    json: bool,
    context_results: Vec<context::ContextResultRef>,
}

// Continues a closed session's provider conversation in a new session. Model, effort, and
// yolo are never inherited from the source manifest; only the options stated here apply.
#[derive(Debug)]
pub(crate) struct ReopenRequest {
    pub(crate) id: String,
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

// Where a reopened session's provider conversation came from: the closed Bridge session and
// the event whose provider session id was passed to the provider's official resume.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ResumedFrom {
    session: String,
    provider_session_id: String,
    event_id: String,
}

// Reopen provenance is an optional `resumed_from` object stored in the schema-1 manifest
// beside the fields every reader knows. It is read through this sibling type so a reader
// that does not know the field keeps parsing the manifest unchanged.
#[derive(Debug, Default, Deserialize)]
struct ReopenProvenance {
    #[serde(default)]
    resumed_from: Option<ResumedFrom>,
}

// The record a winning reopen leaves in its closed source. `reopened_by` is filled once the
// new session exists; until then the marker still excludes every other reopen attempt.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ReopenMarker {
    schema: u32,
    claim: String,
    provider_session_id: String,
    #[serde(default)]
    reopened_by: Option<String>,
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
            | "self-test"
            | "consent"
            | "settings"
            | "tell"
            | "reopen"
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
            | "native-console-host"
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
        "self-test" => self_test::parse(rest),
        "tell" => parse_tell(rest),
        "consent" => Ok(NativeCommand::Consent(rest.to_vec())),
        "settings" => Ok(NativeCommand::Settings(rest.to_vec())),
        "reopen" => parse_reopen(rest),
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
            if !matches!(
                action.as_str(),
                "send" | "close" | "screen" | "dialog" | "window"
            ) {
                bail!("unsupported native console action: {action}");
            }
            let timeout = |timeout_ms: &String| -> Result<u64> {
                let timeout_ms = timeout_ms
                    .parse::<u64>()
                    .context("invalid native console timeout")?;
                if timeout_ms == 0 {
                    bail!("native console timeout must be positive");
                }
                Ok(timeout_ms)
            };
            let (input_name, timeout_ms) = match (action.as_str(), tail) {
                ("send" | "dialog", [input, timeout_ms]) if valid_pending_prompt_name(input) => {
                    (Some(input.clone()), Some(timeout(timeout_ms)?))
                }
                ("window", [timeout_ms]) => (None, Some(timeout(timeout_ms)?)),
                ("close", []) => (None, None),
                ("screen", []) => (None, None),
                _ => bail!("invalid native console control arguments"),
            };
            Ok(NativeCommand::ConsoleControl {
                action: action.clone(),
                id: id.clone(),
                input_name,
                timeout_ms,
            })
        }
        "native-console-host" => {
            // The tab's process does not necessarily inherit the state root, so it is
            // given the session directory itself.
            let directory = PathBuf::from(one_positional(
                rest,
                "native-console-host requires one session directory",
            )?);
            let id = directory
                .file_name()
                .and_then(|name| name.to_str())
                .context("native-console-host requires a session directory")?;
            require_valid_session_id(id)?;
            if !directory.is_absolute() {
                bail!("native-console-host requires an absolute session directory");
            }
            Ok(NativeCommand::ConsoleHost { directory })
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
    let mut context_results = Vec::new();
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
            "--context-result" => context::push_option(
                &mut context_results,
                option_value(options, &mut index, "--context-result")?,
            )?,
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
        context_results,
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
    let mut context_results = Vec::new();
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
            "--context-result" => context::push_option(
                &mut context_results,
                option_value(options, &mut index, "--context-result")?,
            )?,
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
        context_results,
    }))
}

fn parse_reopen(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args
        .split_first()
        .context("reopen requires one closed session id")?;
    require_valid_session_id(id)?;
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
            option => bail!("unknown reopen option: {option}"),
        }
        index += 1;
    }
    let prompt = read_prompt_option(prompt, prompt_file, "reopen")?;
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
    Ok(NativeCommand::Reopen(ReopenRequest {
        id: id.to_owned(),
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
        NativeCommand::Consent(args) => consent::run(&args),
        NativeCommand::Settings(args) => settings::run(&args),
        NativeCommand::Ask(request) => run_ask(request),
        NativeCommand::SelfTest(request) => self_test::run(request),
        NativeCommand::Tell(request) => run_tell(request),
        NativeCommand::Reopen(request) => run_reopen(request),
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
        NativeCommand::ConsoleHost { directory } => run_windows_console_host(&directory),
    }
}

#[cfg(target_os = "windows")]
fn run_windows_console_host(directory: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("no such session directory: {}", directory.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "refusing non-directory session directory: {}",
            directory.display()
        );
    }
    terminal::windows_console_host(directory)
}

#[cfg(not(target_os = "windows"))]
fn run_windows_console_host(_directory: &Path) -> Result<()> {
    bail!("the native Windows console host is only available on Windows")
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
    // A window is looked up right after the console root was started, before the wrapper
    // has recorded itself as the owner. Like a close, it needs only the bound surface.
    if matches!(action, "close" | "window") {
        session.verify_managed_session(id)?;
    } else {
        verify_terminal_surface_ownership(&directory, id, &session)?;
    }
    let input_path = input_name.map(|name| directory.join(name));
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    let submit_count = provider::terminal_submit_count(provider);
    let records = terminal::WindowsSessionRecords {
        directory: &directory,
        root_never_ran: windows_console_root_never_ran(&directory),
    };
    terminal::windows_console_control(
        action,
        &session,
        input_path.as_deref(),
        submit_count,
        timeout_ms.map(Duration::from_millis),
        &records,
    )
}

// The console root runs the wrapper, and the wrapper records itself as the owner before
// it asks to spawn the provider. Without that record and without a spawn attempt in the
// launch receipt, the root has not run its command. A receipt that cannot be read proves
// nothing.
#[cfg(windows)]
fn windows_console_root_never_ran(directory: &Path) -> bool {
    !directory.join(SESSION_OWNER_FILE).exists()
        && match launch::read(directory) {
            Ok(None) => true,
            Ok(Some(record)) => record.phase == launch::Phase::Pending,
            Err(_) => false,
        }
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
    // Attached results are resolved and pinned before any session, claim, receipt, terminal,
    // or delivery exists, so a failed resolution changes nothing.
    let attached = context::resolve(&request.context_results)?;
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
        prompt: native_delegation_prompt(
            &delegation_source(),
            &attached.prompt_with_attachments(&request.prompt),
        ),
    })?;
    consent::prepare(&created.directory, &request.workspace)?;
    launch_created_session(
        SessionLaunch {
            created,
            provider: request.provider,
            terminal_kind,
            deadline,
            timeout: request.timeout,
            detach: request.detach,
            json: request.json,
            context_sources: &attached.sources,
            result_extra: serde_json::Map::new(),
            resumed_from: None,
        },
        address,
    )
}

// A created session that is ready to be launched. `ask` and `reopen` share everything from
// the initial turn claim onwards: terminal opening, launch-failure cleanup, initial prompt
// delivery, and result waiting. `result_extra` carries command-specific fields into the
// emitted JSON response.
struct SessionLaunch<'a> {
    created: CreatedSession,
    provider: FirstPartyCli,
    terminal_kind: terminal::TerminalKind,
    deadline: Instant,
    timeout: Duration,
    detach: bool,
    json: bool,
    context_sources: &'a [requests::ContextSource],
    result_extra: serde_json::Map<String, serde_json::Value>,
    // Set for `reopen`: the provider conversation the new session continues. Its holder
    // check runs after launch, once the process exists, and again immediately before the
    // first prompt is sent.
    resumed_from: Option<ResumedFrom>,
}

fn launch_created_session(
    launch: SessionLaunch<'_>,
    address: &mut Option<(String, String)>,
) -> Result<()> {
    let SessionLaunch {
        created,
        provider,
        terminal_kind,
        deadline,
        timeout,
        detach,
        json,
        context_sources,
        result_extra,
        resumed_from,
    } = launch;
    let mut initial_claim = acquire_turn_claim_with_context(&created.directory, context_sources)?;
    let expected_claim_token = initial_claim.token.clone();
    let receipt = initial_claim.receipt.clone();
    *address = Some((created.id.clone(), receipt.request_id.clone()));
    let launch_deadline = launch::begin(&created.directory, &expected_claim_token, deadline)?;
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
    let bridge_command = launch::install_script(&created.directory, &bridge_command)?;
    let terminal_handle_path = created.directory.join(TERMINAL_HANDLE_FILE);
    initial_claim.retain_in_place();
    let terminal_session = match terminal::open_bound_tab(
        terminal_kind,
        &bridge_command,
        &created.directory,
        launch_deadline,
        |session| {
            session.managed_session_id = Some(created.id.clone());
            write_json_atomic(&terminal_handle_path, session)
        },
        || remove_file_if_present(&terminal_handle_path),
    ) {
        Ok(session) => session,
        Err(error) => {
            let _ = launch::fail(
                &created.directory,
                &format!("terminal launch failed: {error:#}"),
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
    // Once the terminal accepted the wrapper command, only a fenced launch failure may
    // release this claim. Detached callers also wait for provider startup, not its result.
    initial_claim.retain_in_place();
    launch::wait(&created.directory, &terminal_session, launch_deadline)?;
    // A trust response is separate from model input. Only verified consent and an
    // adapter-recognized exact workspace dialog may produce one guarded response.
    consent::complete_launch(&created.directory, provider, &terminal_session, deadline)?;
    // The existing delivery paths own rollback/uncertainty after confirmed startup.
    initial_claim.retained = false;
    let initial_prompt_transport = provider::initial_prompt_transport(provider);
    let mut expected_turn_id = None;
    if initial_prompt_transport == provider::InitialPromptTransport::TerminalPasteAfterLaunch {
        let mut delivery_may_have_occurred = false;
        let delivery = (|| -> Result<()> {
            wait_for_status(
                &created.directory,
                "awaiting-initial-input",
                deadline,
                timeout,
            )?;
            let readiness_delay = initial_prompt_delay_within_budget(
                deadline,
                provider::initial_prompt_ready_delay(provider),
                timeout,
            )?;
            thread::sleep(readiness_delay);
            verify_reopened_conversation_exclusive(
                provider,
                &created.directory,
                resumed_from.as_ref(),
                deadline,
                ResumedHolderCheck::AfterLaunch,
            )?;
            let initial_prompt_path = created.directory.join("initial-prompt.txt");
            let initial_prompt = fs::read_to_string(&initial_prompt_path)
                .context("failed to read the preserved initial prompt")?;
            let initial_prompt =
                provider::terminal_initial_prompt(provider, &created.directory, &initial_prompt)?;
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
                timeout,
            )?;
            let send_timeout = remaining_turn_timeout(deadline, timeout)?;
            provider::validate_terminal_send_budget(provider, terminal_session.kind, send_timeout)?;
            verify_reopened_conversation_exclusive(
                provider,
                &created.directory,
                resumed_from.as_ref(),
                deadline,
                ResumedHolderCheck::BeforeInitialDelivery,
            )?;
            update_status(&created.directory, "working", None, None)?;
            match provider::send_initial_prompt(
                provider,
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
            let error = close_surface_after_reopen_verification_failure(
                &created.directory,
                &created.id,
                error,
            );
            return Err(error).with_context(|| {
                format!(
                    "failed to deliver the initial prompt to {} session {}",
                    provider.as_str(),
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
                timeout,
            )?;
            let readiness_delay = initial_prompt_delay_within_budget(
                deadline,
                provider::initial_prompt_ready_delay(provider),
                timeout,
            )?;
            thread::sleep(readiness_delay);
            verify_reopened_conversation_exclusive(
                provider,
                &created.directory,
                resumed_from.as_ref(),
                deadline,
                ResumedHolderCheck::AfterLaunch,
            )?;
            let request_id = provider::new_cross_session_turn_id(provider)?;
            let prompt_path = created.directory.join("initial-prompt.txt");
            let prompt = fs::read_to_string(&prompt_path)
                .context("failed to read the preserved initial prompt")?;
            let bridge_executable =
                std::env::current_exe().context("failed to locate agent-bridge executable")?;
            remaining_turn_timeout(deadline, timeout)?;
            verify_reopened_conversation_exclusive(
                provider,
                &created.directory,
                resumed_from.as_ref(),
                deadline,
                ResumedHolderCheck::BeforeInitialDelivery,
            )?;
            update_status(&created.directory, "working", None, None)?;
            match provider::send_cross_session_message(
                provider,
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
                    let error = failure.into_error();
                    record_cross_session_delivery_uncertainty(
                        &created.directory,
                        &mut initial_claim,
                        &error,
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
                let error = close_surface_after_reopen_verification_failure(
                    &created.directory,
                    &created.id,
                    error,
                );
                return Err(error).with_context(|| {
                    format!(
                        "failed to deliver the initial prompt to {} session {}",
                        provider.as_str(),
                        created.id
                    )
                });
            }
        }
    } else {
        initial_claim.retain();
    }

    if detach {
        return emit_session_result_with(
            json,
            &created.id,
            &terminal_session,
            provider,
            &receipt,
            None,
            &result_extra,
        );
    }

    let event = wait_for_event_for_turn_until(
        &created.directory,
        0,
        expected_turn_id.as_deref(),
        Some(&expected_claim_token),
        deadline,
        timeout,
    )
    .with_context(|| {
        format!(
            "session {} remains open in {}; use `agent-bridge sessions` to inspect it",
            created.id,
            terminal_session.kind.display_name()
        )
    })?;
    emit_session_result_with(
        json,
        &created.id,
        &terminal_session,
        provider,
        &receipt,
        Some(&event),
        &result_extra,
    )
}

// A reopen that fails a pre-launch gate. The gate name reaches the JSON response so a caller
// can tell a refused reopen from a launch or delivery failure of the new session.
#[derive(Debug)]
struct ReopenRefusal {
    gate: &'static str,
    detail: String,
}

impl std::fmt::Display for ReopenRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "reopen refused ({}): {}", self.gate, self.detail)
    }
}

impl std::error::Error for ReopenRefusal {}

fn reopen_refusal(gate: &'static str, detail: String) -> anyhow::Error {
    anyhow::Error::new(ReopenRefusal { gate, detail })
}

fn reopen_refusal_gate(error: &anyhow::Error) -> Option<&'static str> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ReopenRefusal>())
        .map(|refusal| refusal.gate)
}

const REOPEN_LAUNCH_GATE: &str = "provider-unsupported";
const REOPEN_CONFLICT_GATE: &str = "reopen-conflict";
const REOPEN_VERIFICATION_FAILED_GATE: &str = "reopen-verification-failed";

// The gates a reopen can fail after its session exists. Each one leaves the new session
// failed with no prompt delivered. The source's reopen marker is released again only once
// the refused launch provably cannot hold the conversation (`RefusedLaunchCleanup`).
const REOPEN_POST_CREATION_GATES: [&str; 3] = [
    REOPEN_LAUNCH_GATE,
    REOPEN_CONFLICT_GATE,
    REOPEN_VERIFICATION_FAILED_GATE,
];

// The only phase whose refusals are persisted. The record exists so the reopen command can
// name a gate that failed in the launch wrapper, in another process; every writer of it is
// a launch-phase gate (the pre-spawn recheck, the post-launch holder check, and the holder
// check immediately before the initial prompt is sent), and the reader accepts nothing
// else. A refused later `tell` keeps its reason in the session status and its own response
// only: it must never be mistaken for a refusal of the initial delivery, whose uncertain
// outcome is decided from this record.
const REOPEN_REFUSAL_LAUNCH_PHASE: &str = "launch";

// `cleanup` is written by a marker settlement that found the refused launch may still hold
// the conversation (a failed close of a spawned process). It is a note for `doctor` and the
// next reopen attempt; the release decision itself is always taken from the session's live
// records, never from this field.
const REOPEN_REFUSAL_CLEANUP_PENDING: &str = "pending";

#[derive(Debug, Deserialize, Serialize)]
struct RecordedReopenRefusal {
    schema: u32,
    phase: String,
    gate: String,
    detail: String,
    created_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup_detail: Option<String>,
}

// Records a launch-phase gate refusal that happened after the new session existed, then
// returns the typed refusal. Only the launch phase writes this record; follow-up refusals
// go through `record_follow_up_refusal`, which touches the session status alone.
fn record_reopen_refusal(directory: &Path, gate: &'static str, detail: String) -> anyhow::Error {
    let record = RecordedReopenRefusal {
        schema: 2,
        phase: REOPEN_REFUSAL_LAUNCH_PHASE.to_owned(),
        gate: gate.to_owned(),
        detail: detail.clone(),
        created_unix_ms: unix_ms(),
        cleanup: None,
        cleanup_detail: None,
    };
    if let Err(error) = write_json_atomic(&directory.join(REOPEN_REFUSAL_FILE), &record) {
        return reopen_refusal(
            gate,
            format!("{detail}; the refusal record could not be written: {error:#}"),
        );
    }
    reopen_refusal(gate, detail)
}

// The launch-phase refusal recorded in a reopened session, if there is one. A record of any
// other schema or phase is not a launch refusal and yields nothing, so the caller treats the
// launch as not refused.
fn read_reopen_launch_refusal(directory: &Path) -> Option<RecordedReopenRefusal> {
    let text = read_regular_text_if_present(&directory.join(REOPEN_REFUSAL_FILE))
        .ok()
        .flatten()?;
    let record: RecordedReopenRefusal = serde_json::from_str(&text).ok()?;
    (record.schema == 2 && record.phase == REOPEN_REFUSAL_LAUNCH_PHASE).then_some(record)
}

fn read_reopen_refusal_gate(directory: &Path) -> Option<String> {
    read_reopen_launch_refusal(directory).map(|record| record.gate)
}

// Notes in the launch refusal record that the marker settlement could not establish that
// the refused launch is gone. A missing record (its write failed when the refusal was
// decided) leaves nothing to annotate; the retention itself does not depend on the note.
fn record_reopen_refusal_cleanup_pending(directory: &Path, reason: &str) -> Result<()> {
    let Some(mut record) = read_reopen_launch_refusal(directory) else {
        return Ok(());
    };
    record.cleanup = Some(REOPEN_REFUSAL_CLEANUP_PENDING.to_owned());
    record.cleanup_detail = Some(reason.to_owned());
    write_json_atomic(&directory.join(REOPEN_REFUSAL_FILE), &record)
}

// The provider process a launch wrapper spawned for its session (`PROVIDER_PROCESS_FILE`).
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProviderProcessRecord {
    schema: u32,
    managed_session_id: String,
    pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    windows_process_identity: Option<terminal::WindowsProcessIdentity>,
    spawned_unix_ms: u128,
}

// Records the spawned provider process before the session leaves its launch state. The
// caller still holds the child handle, so on Windows the pid cannot have been reused
// between the spawn and the identity query. The record names the process Bridge spawned:
// for a native executable that is the provider itself; for a `.ps1` or `.cmd` shim it is
// the shim, which waits on the provider and whose own survival is what the record can
// verify.
fn record_provider_process(
    directory: &Path,
    managed_session_id: &str,
    child: &std::process::Child,
) -> Result<()> {
    let pid = child.id();
    #[cfg(windows)]
    let windows_process_identity = Some(
        terminal::windows_process_identity(pid)
            .with_context(|| format!("failed to record the identity of provider process {pid}"))?,
    );
    #[cfg(not(windows))]
    let windows_process_identity = None;
    write_json_atomic(
        &directory.join(PROVIDER_PROCESS_FILE),
        &ProviderProcessRecord {
            schema: 1,
            managed_session_id: managed_session_id.to_owned(),
            pid,
            windows_process_identity,
            spawned_unix_ms: unix_ms(),
        },
    )
    .context("failed to record the spawned provider process")
}

// Why a recorded provider process is known to be gone. Identity mismatches are only
// observable where the reopen slice runs (native Windows); other targets never build one.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum ProviderProcessGone {
    // Its pid is no longer alive.
    Exited,
    // Its pid is alive but belongs to a different process: the pid was reused after the
    // recorded process exited (Windows creation time or executable path differ).
    IdentityMismatch(&'static str),
}

// What can be observed about a recorded provider process now.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum ProviderProcessObservation {
    Gone(ProviderProcessGone),
    // The pid is alive and, where an identity was recorded, still carries it.
    Alive,
    // The pid is alive (or its liveness cannot be denied) but its identity could not be
    // inspected, so neither survival nor reuse is established.
    Unknown(String),
}

// Reopen-local observation of a recorded provider process. Unlike the owner observation
// used by dead-owner repair (`query::observe_owner_record`), it keeps a confirmed identity
// mismatch apart from a failure to inspect: the first is a pid reused by another process
// and releases the marker, the second retains it.
fn observe_provider_process(record: &ProviderProcessRecord) -> ProviderProcessObservation {
    #[cfg(windows)]
    if let Some(identity) = &record.windows_process_identity {
        return classify_provider_process_identity(
            record.pid,
            terminal::check_windows_process_identity(record.pid, identity),
        );
    }
    if process_is_alive(record.pid) {
        ProviderProcessObservation::Alive
    } else {
        ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
    }
}

// Maps the typed Windows identity check onto the observation. An inspection failure is
// `Gone` only when the pid is not alive at all; a pid that is alive (or access-denied, which
// liveness treats as alive) but uninspectable stays `Unknown`.
#[cfg(any(windows, test))]
fn classify_provider_process_identity(
    pid: u32,
    check: Result<terminal::WindowsProcessIdentityCheck>,
) -> ProviderProcessObservation {
    match check {
        Ok(terminal::WindowsProcessIdentityCheck::Matches) => ProviderProcessObservation::Alive,
        Ok(terminal::WindowsProcessIdentityCheck::Mismatch(reason)) => {
            ProviderProcessObservation::Gone(ProviderProcessGone::IdentityMismatch(reason))
        }
        Err(_) if !process_is_alive(pid) => {
            ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
        }
        Err(error) => ProviderProcessObservation::Unknown(format!("{error:#}")),
    }
}

// Whether a launch-refused reopened session can still hold the provider conversation. The
// source's reopen marker is released only on the two verified outcomes; `Pending` keeps it
// consumed, because a refused launch whose provider process survived (a close that failed
// or was never performed, a process that ignored the console close, a process that never
// registered) is exactly the second live writer the gate exists to keep out, and the
// registry scan of the next reopen would not see an unregistered one. Neither the launch
// wrapper's liveness nor a closed surface is evidence on its own: Windows does not end a
// child with its parent, dead-owner repair marks a session closed without any terminal
// close, and a provider can outlive the console it was started in.
#[derive(Debug, PartialEq)]
enum RefusedLaunchCleanup {
    // The pre-spawn recheck refused and no provider process was recorded: the launch
    // wrapper started none.
    NoProcessSpawned,
    // The provider process the launch wrapper recorded is verified gone: its pid is dead
    // or the pid is alive under a different identity. `surface_closed` adds that the
    // refused session's surface was consumed by a close; it is reported, never relied on.
    ProviderProcessGone {
        pid: u32,
        evidence: ProviderProcessGone,
        surface_closed: bool,
    },
    // Neither of the above can be established from the session's records.
    Pending(String),
}

impl RefusedLaunchCleanup {
    fn releases_marker(&self) -> bool {
        !matches!(self, Self::Pending(_))
    }
}

impl std::fmt::Display for RefusedLaunchCleanup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProcessSpawned => write!(formatter, "no provider process was spawned"),
            Self::ProviderProcessGone {
                pid,
                evidence,
                surface_closed,
            } => {
                write!(formatter, "provider process {pid} is verified gone (")?;
                match evidence {
                    ProviderProcessGone::Exited => write!(formatter, "it has exited")?,
                    ProviderProcessGone::IdentityMismatch(reason) => {
                        write!(
                            formatter,
                            "the pid now belongs to another process: {reason}"
                        )?;
                    }
                }
                write!(formatter, ")")?;
                if *surface_closed {
                    write!(formatter, " and the refused session's surface was closed")?;
                }
                Ok(())
            }
            Self::Pending(reason) => {
                write!(
                    formatter,
                    "the refused launch may still hold the conversation: {reason}"
                )
            }
        }
    }
}

// Read-only. Establishes, from the refused session's own records, whether the launch that
// was refused under `gate` can still hold the conversation. A session that accepts prompts
// or is working is never a refused launch, whatever its record says. The pre-spawn gate
// proves nothing by itself: only the absence of a provider process record shows that no
// process was spawned, and a record that exists is verified like any other.
fn refused_launch_cleanup(refused_directory: &Path, gate: &str) -> Result<RefusedLaunchCleanup> {
    let refused_session = refused_directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    // The refused session's own status decides whether cleanup is even possible, so it is
    // read like the other evidence here: a link at `status.json` is refused, not followed.
    let status_path = refused_directory.join("status.json");
    let state = read_regular_status_if_present(&status_path)?
        .with_context(|| format!("failed to read {}", status_path.display()))?
        .state;
    if session_accepts_prompt(&state) || state == "working" {
        return Ok(RefusedLaunchCleanup::Pending(format!(
            "refused session {refused_session} is {state}"
        )));
    }
    let record = query::optional_json::<ProviderProcessRecord>(
        &refused_directory.join(PROVIDER_PROCESS_FILE),
    )?;
    let Some(record) = record else {
        if gate == REOPEN_LAUNCH_GATE {
            return Ok(RefusedLaunchCleanup::NoProcessSpawned);
        }
        return Ok(RefusedLaunchCleanup::Pending(format!(
            "refused session {refused_session} is {state} with no provider process record ({PROVIDER_PROCESS_FILE}), so the provider process it spawned cannot be verified gone"
        )));
    };
    if record.schema != 1 || record.managed_session_id != refused_session {
        return Ok(RefusedLaunchCleanup::Pending(format!(
            "the provider process record of refused session {refused_session} names {:?} (schema {})",
            record.managed_session_id, record.schema
        )));
    }
    let evidence = match observe_provider_process(&record) {
        ProviderProcessObservation::Gone(evidence) => evidence,
        ProviderProcessObservation::Alive => {
            return Ok(RefusedLaunchCleanup::Pending(format!(
                "provider process {} of refused session {refused_session} is still running",
                record.pid
            )));
        }
        ProviderProcessObservation::Unknown(error) => {
            return Ok(RefusedLaunchCleanup::Pending(format!(
                "provider process {} of refused session {refused_session} could not be verified: {error}",
                record.pid
            )));
        }
    };
    let surface_closed = state == "closed"
        && is_regular_file(&refused_directory.join(TERMINAL_TOMBSTONE_FILE))?
        && read_regular_status_if_present(&refused_directory.join(CLOSED_STATUS_FILE))?
            .is_some_and(|closed| closed.state == "closed");
    Ok(RefusedLaunchCleanup::ProviderProcessGone {
        pid: record.pid,
        evidence,
        surface_closed,
    })
}

// Which boundary a resumed session's holder check runs at. The provider grants no
// exclusive hold on a conversation, so the check is best-effort detection repeated at every
// point Bridge is about to act on the conversation, never a reservation of it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ResumedHolderCheck {
    // In the initial-prompt readiness window: the reopened process exists and has not
    // received a prompt. The adapter waits for the provider's own registration of it.
    AfterLaunch,
    // Immediately before the initial prompt is sent to the already registered process. The
    // adapter answers from the registry as it is now.
    BeforeInitialDelivery,
    // Immediately before a later `tell` is sent. Same answer as the initial check, but a
    // refusal here is a follow-up refusal: it is never persisted as a launch refusal.
    BeforeFollowUp,
}

impl ResumedHolderCheck {
    // The launch-phase checks persist their refusal for the reopen command; see
    // `REOPEN_REFUSAL_LAUNCH_PHASE`.
    fn persists_refusal(self) -> bool {
        match self {
            Self::AfterLaunch | Self::BeforeInitialDelivery => true,
            Self::BeforeFollowUp => false,
        }
    }
}

// The holder check of a reopened session. The adapter reports every other live holder of the
// conversation; a non-empty answer is a detected conflict and refuses under
// `reopen-conflict`. Any failure to complete the check (unreadable registry, a live record
// that cannot be verified, an uninspectable process, a duplicate managed name, a registration
// that never came) refuses under `reopen-verification-failed`: an unverifiable conversation
// is treated as shared, never as exclusive. At the launch-phase boundaries both refusals are
// recorded in the session so the gate survives the process boundary; a follow-up refusal is
// returned unrecorded. The recorded detail states only what was detected; whether the new
// surface was then closed is reported by the caller once that outcome is known. A foreign
// resume that registers between two checks is not detected until the next one.
fn verify_reopened_conversation_exclusive(
    provider: FirstPartyCli,
    directory: &Path,
    resumed_from: Option<&ResumedFrom>,
    deadline: Instant,
    check: ResumedHolderCheck,
) -> Result<()> {
    let Some(resumed_from) = resumed_from else {
        return Ok(());
    };
    let refuse = |gate: &'static str, detail: String| {
        if check.persists_refusal() {
            record_reopen_refusal(directory, gate, detail)
        } else {
            reopen_refusal(gate, detail)
        }
    };
    let others = match provider::other_resumed_conversation_holders(
        provider,
        provider::ResumedSessionContext {
            directory,
            provider_session_id: &resumed_from.provider_session_id,
            deadline,
            wait_for_registration: check == ResumedHolderCheck::AfterLaunch,
        },
    ) {
        Ok(others) => others,
        Err(error) => {
            return Err(refuse(
                REOPEN_VERIFICATION_FAILED_GATE,
                format!(
                    "could not verify that the reopened {} conversation {} has no other live holder: {error:#}; no prompt was delivered",
                    provider.as_str(),
                    resumed_from.provider_session_id
                ),
            ));
        }
    };
    if others.is_empty() {
        return Ok(());
    }
    Err(refuse(
        REOPEN_CONFLICT_GATE,
        format!(
            "{} conversation {} is also held by live {} process(es) {}; no prompt was delivered",
            provider.as_str(),
            resumed_from.provider_session_id,
            provider.as_str(),
            others
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

// A failed holder check, whether a detected conflict or a verification the adapter could
// not complete, is the launch failure that also closes the new surface: the reopened
// process is a second live writer of the conversation (or cannot be shown not to be), and
// leaving it open would keep the interleaving the gate exists to detect. Every other launch
// failure keeps the existing behavior of marking only the new session failed. Only the new
// session's own handle is ever closed; the source keeps its tombstone. The returned error
// reports the close outcome only after it is known.
fn close_surface_after_reopen_verification_failure(
    directory: &Path,
    id: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    close_surface_after_reopen_verification_failure_with(id, error, |detected| {
        close_session_surface(directory, id, Some(format!("{detected:#}")))
    })
}

// `close` receives the detected refusal so the closed status can keep it as its reason.
fn close_surface_after_reopen_verification_failure_with(
    id: &str,
    error: anyhow::Error,
    close: impl FnOnce(&anyhow::Error) -> Result<()>,
) -> anyhow::Error {
    if !matches!(
        reopen_refusal_gate(&error),
        Some(REOPEN_CONFLICT_GATE | REOPEN_VERIFICATION_FAILED_GATE)
    ) {
        return error;
    }
    match close(&error) {
        Ok(()) => error.context(format!(
            "the reopened session {id} was closed before any prompt was delivered"
        )),
        Err(close_error) => error.context(format!(
            "the reopened session {id} could not be closed and may still hold the conversation: {close_error:#}"
        )),
    }
}

// Closes a managed session's own visible surface exactly as an explicit `close-session`
// does, through the same terminal-close authority checks. `reason` is kept in the closed
// status when the close itself reports nothing, so a session closed because of a detected
// conflict still says why.
fn close_session_surface(directory: &Path, id: &str, reason: Option<String>) -> Result<()> {
    close_repaired_session_state_with_reason(directory, reason, |session| {
        let has_native_owner = verify_terminal_close_authority(directory, id, session)?;
        #[cfg(target_os = "macos")]
        if has_native_owner && session.kind == terminal::TerminalKind::AppleTerminal {
            terminate_apple_terminal_owner(directory, id, session)?;
        }
        #[cfg(not(target_os = "macos"))]
        let _ = has_native_owner;
        terminal::close_session(session)
    })
}

// What the read-only gates established about a closed source session.
#[derive(Debug)]
struct ReopenSource {
    manifest: SessionManifest,
    provider: FirstPartyCli,
    provider_session_id: String,
    event_id: String,
}

fn run_reopen(request: ReopenRequest) -> Result<()> {
    let json = request.json;
    let source = request.id.clone();
    let mut address = None;
    let outcome = run_reopen_inner(request, &mut address);
    match address {
        Some((session, request_id)) => {
            let (outcome, gate) =
                settle_reopen_outcome(session_directory, &source, &session, outcome);
            let mut extra = serde_json::Map::new();
            extra.insert("source_session".to_owned(), serde_json::json!(source));
            extra.insert("gate".to_owned(), serde_json::json!(gate));
            finish_request_with_extra(outcome, json, &session, &request_id, extra)
        }
        None => {
            if let Err(error) = &outcome
                && json
            {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "schema_version": 1,
                        "ok": false,
                        "source_session": source,
                        "session": null,
                        "request_id": null,
                        "gate": reopen_refusal_gate(error),
                        "error": format!("{error:#}"),
                    }))?
                );
            }
            outcome
        }
    }
}

// Decides, once the reopened session exists, whether the reopen was refused at a
// post-creation gate and whether that refusal releases the source's reopen marker. The gate
// is typed in this process (the post-launch and pre-initial-delivery holder checks) or
// recorded by the launch wrapper in the new session (the pre-spawn recheck); either way the
// response names it. Only a launch-phase refusal counts: the initial delivery can complete
// while its messenger is still settling, a later `tell` can then be refused and the session
// closed, and the initial messenger can finally report only that delivery is uncertain.
// That `tell` refusal lives in the session status, never in the launch refusal record, so
// the uncertain outcome finds no gate here and the marker stays consumed: a prompt may
// have reached the conversation, and a second reopen must not be permitted on the strength
// of a refusal that was not the launch's.
fn settle_reopen_outcome(
    session_directory: impl Fn(&str) -> Result<PathBuf>,
    source: &str,
    session: &str,
    outcome: Result<()>,
) -> (Result<()>, Option<String>) {
    let gate = outcome.as_ref().err().and_then(|error| {
        reopen_refusal_gate(error).map(str::to_owned).or_else(|| {
            session_directory(session)
                .ok()
                .and_then(|directory| read_reopen_refusal_gate(&directory))
        })
    });
    let outcome = match gate.as_deref() {
        Some(gate) if REOPEN_POST_CREATION_GATES.contains(&gate) => {
            match (session_directory(source), session_directory(session)) {
                (Ok(source_directory), Ok(refused_directory)) => {
                    release_reopen_marker_after_refusal(
                        &source_directory,
                        &refused_directory,
                        gate,
                        outcome,
                    )
                }
                (Err(error), _) | (_, Err(error)) => outcome.context(format!(
                    "the reopen marker of source session {source} was not released: {error:#}"
                )),
            }
        }
        _ => outcome,
    };
    (outcome, gate)
}

fn run_reopen_inner(request: ReopenRequest, address: &mut Option<(String, String)>) -> Result<()> {
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    let terminal_kind = terminal::select(request.terminal)?;
    let source_directory = session_directory(&request.id)?;
    // Every check against the source is read-only. Pending-completion recovery and dead-owner
    // repair are never run on it: a closed session has nothing to converge, and reopen must not
    // alter the record it continues from.
    let source = inspect_reopen_source(&source_directory, &request.id)?;
    provider::verify_reopen_available(source.provider, &source.provider_session_id)
        .map_err(|error| reopen_refusal("provider-unsupported", format!("{error:#}")))?;
    let workspace = source.manifest.workspace.canonicalize().with_context(|| {
        format!(
            "source workspace does not exist or cannot be resolved: {}",
            source.manifest.workspace.display()
        )
    })?;
    if !workspace.is_dir() {
        bail!(
            "source workspace is not a directory: {}",
            workspace.display()
        );
    }
    let provider_path = resolve_provider(source.provider)?;
    let provider_version =
        check_provider_version_until(source.provider, &provider_path, Some(deadline))?;
    let requested_title = request.title.unwrap_or_else(|| {
        let workspace_name = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        format!(
            "{} · {workspace_name} (reopened {})",
            source.provider.as_str(),
            request.id
        )
    });
    let title = sanitize_title(&requested_title)?;
    let marker = claim_reopen_marker(&source_directory, &request.id, &source.provider_session_id)?;
    let created = create_session(SessionSpec {
        provider: source.provider,
        provider_path,
        provider_version,
        workspace,
        title,
        model: request.model,
        effort: request.effort,
        yolo: request.yolo,
        prompt: native_delegation_prompt(&delegation_source(), &request.prompt),
    })?;
    let resumed_from = ResumedFrom {
        session: request.id.clone(),
        provider_session_id: source.provider_session_id.clone(),
        event_id: source.event_id.clone(),
    };
    if let Err(error) = record_resumed_from(&created.directory, &created.manifest, &resumed_from)
        .and_then(|()| marker.finalize(&created.id))
    {
        let _ = update_status(
            &created.directory,
            "failed",
            None,
            Some(format!("{error:#}")),
        );
        return Err(error).with_context(|| {
            format!(
                "failed to record reopen provenance for session {}",
                created.id
            )
        });
    }
    let mut result_extra = serde_json::Map::new();
    result_extra.insert(
        "source_session".to_owned(),
        serde_json::Value::String(request.id.clone()),
    );
    result_extra.insert(
        "resumed_from".to_owned(),
        serde_json::to_value(&resumed_from)?,
    );
    launch_created_session(
        SessionLaunch {
            created,
            provider: source.provider,
            terminal_kind,
            deadline,
            timeout: request.timeout,
            detach: request.detach,
            json: request.json,
            context_sources: &[],
            result_extra,
            resumed_from: Some(resumed_from),
        },
        address,
    )
}

// A reopen refused after its session existed delivered nothing to the conversation: the
// pre-spawn recheck started no process, and both post-launch gates refuse before the first
// prompt and close the new surface. Bridge therefore established no conversation writer,
// and the source's reopen marker is released so the source can be reopened again once the
// cause is gone. The refused session keeps its `resumed_from` as provenance. Release
// requires that the marker still names the refused session and that the refused launch
// provably cannot hold the conversation (`refused_launch_cleanup`): a spawned provider
// process may survive the console close and the wrapper without ever registering, so the
// registry gate of the next reopen would not catch it. Until that process is verified
// gone the marker stays consumed, the refusal record is annotated with `cleanup:
// "pending"`, and the next reopen attempt reconciles the marker
// (`verify_reopen_source_is_closed`).
fn release_reopen_marker_after_refusal(
    source_directory: &Path,
    refused_directory: &Path,
    gate: &str,
    outcome: Result<()>,
) -> Result<()> {
    let refused_session = refused_directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let released = (|| -> Result<()> {
        let cleanup = refused_launch_cleanup(refused_directory, gate)?;
        if let RefusedLaunchCleanup::Pending(reason) = &cleanup {
            let note = record_reopen_refusal_cleanup_pending(refused_directory, reason)
                .err()
                .map_or(String::new(), |error| {
                    format!("; the refusal record could not be annotated: {error:#}")
                });
            bail!("{cleanup}{note}");
        }
        let marker_path = source_directory.join(REOPEN_MARKER_FILE);
        let _lock = lock_turn_claim(&source_directory.join(TURN_CLAIM_FILE))?;
        let Some(text) = read_regular_text_if_present(&marker_path)? else {
            return Ok(());
        };
        let marker: ReopenMarker = serde_json::from_str(&text).context("invalid reopen marker")?;
        if marker.reopened_by.as_deref() != Some(refused_session) {
            bail!(
                "reopen marker names {:?}, not the refused session {refused_session}",
                marker.reopened_by
            );
        }
        remove_file_if_present(&marker_path)
    })();
    let Err(release_error) = released else {
        return outcome;
    };
    let release_error = release_error.context(format!(
        "the reopen marker of source session {} was not released",
        source_directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    ));
    match outcome {
        Ok(()) => Err(release_error),
        Err(error) => Err(error.context(format!("{release_error:#}"))),
    }
}

// Read-only gates on the closed source: it is closed with its tombstone and nothing of its
// lifecycle is left open, every request record resolves to a recorded event, and a provider
// event supplies the conversation identity. Each refusal names its gate. Receipts are
// validated before the identity is read, so a corrupt newest event refuses under
// `request-unresolved` whether or not a receipt points at it.
fn inspect_reopen_source(directory: &Path, id: &str) -> Result<ReopenSource> {
    let manifest = read_manifest(directory)?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    verify_reopen_source_is_closed(directory, id)?;
    // The lifecycle readers' contract for `events`: it is a real directory inside the
    // session, or absent, before anything under it is opened. A link or a non-directory
    // planted there would carry the reads below outside the session, and the shared
    // listing would present it as "no events", which the identity gate would then report
    // as a source without an identity. Neither can prove what the source delivered. An
    // absent directory is not rejected here: it holds no event, so the receipt and identity
    // gates below refuse for what is actually missing.
    events_directory_state(directory).map_err(|error| {
        reopen_refusal(
            "request-unresolved",
            format!("the recorded results of session {id} cannot be read: {error:#}"),
        )
    })?;
    let index = requests::list(directory)?;
    if index.unreadable > 0 {
        return Err(reopen_refusal(
            "request-unresolved",
            format!(
                "session {id} has {} unreadable request record(s); their delivery outcome cannot be verified",
                index.unreadable
            ),
        ));
    }
    // Every receipt must resolve to a readable, well-formed event of this provider. A
    // receipt whose event is missing, empty, malformed, or from another provider cannot
    // prove what that request delivered, whatever the latest event says.
    for receipt in &index.receipts {
        let event_path = directory.join("events").join(&receipt.event_file);
        if !is_regular_file(&event_path)? {
            return Err(reopen_refusal(
                "request-unresolved",
                format!(
                    "request {} of session {id} has no recorded result; its delivery outcome is uncertain",
                    receipt.request_id
                ),
            ));
        }
        let event = read_json::<SessionEvent>(&event_path).map_err(|error| {
            reopen_refusal(
                "request-unresolved",
                format!(
                    "request {} of session {id} points at recorded result {} that cannot be read: {error:#}",
                    receipt.request_id, receipt.event_file
                ),
            )
        })?;
        if event.provider != provider.as_str() {
            return Err(reopen_refusal(
                "request-unresolved",
                format!(
                    "request {} of session {id} points at recorded result {} of provider {} instead of {}",
                    receipt.request_id,
                    receipt.event_file,
                    event.provider,
                    provider.as_str()
                ),
            ));
        }
    }
    let (event_id, provider_session_id) =
        latest_provider_event_identity(directory, provider, id)?.ok_or_else(|| {
            reopen_refusal(
                "source-identity-missing",
                format!(
                    "session {id} has no {} event that records a provider session id; a session whose only turn failed cannot be reopened",
                    provider.as_str()
                ),
            )
        })?;
    Ok(ReopenSource {
        manifest,
        provider,
        provider_session_id,
        event_id,
    })
}

// A reopen marker that names a session whose launch was refused and whose cleanup is now
// verified. The marker no longer excludes anything: the reopen it recorded delivered no
// prompt and its process is provably gone, so the next claim under the source lock removes
// it before writing its own.
#[derive(Debug)]
struct StaleReopenMarker {
    refused_session: String,
    gate: String,
    cleanup: RefusedLaunchCleanup,
}

// Read-only. The source is closed with its tombstone, nothing of its lifecycle is left
// open, and any reopen marker it carries is either stale (returned, so the claim can
// release it) or refuses under `already-reopened` naming the blocking condition. A marker
// is stale only when the session it names carries a durable launch-phase refusal and that
// refused launch is verified unable to hold the conversation (`refused_launch_cleanup`).
// This is how a marker whose parent reopen crashed before settlement, or whose launch
// wrapper recorded its refusal only after the parent timed out, is reconciled: nothing is
// inferred from the absence of records, and a marker that names a session without a launch
// refusal, or with a launch refusal whose process may survive, stays consumed.
fn verify_reopen_source_is_closed(directory: &Path, id: &str) -> Result<Option<StaleReopenMarker>> {
    let closed = read_regular_status_if_present(&directory.join(CLOSED_STATUS_FILE))?;
    let status = read_regular_status_if_present(&directory.join("status.json"))?;
    let state = status
        .as_ref()
        .map_or("unknown", |status| status.state.as_str());
    if state != "closed"
        || closed
            .as_ref()
            .is_none_or(|closed| closed.state != "closed")
    {
        return Err(reopen_refusal(
            "source-not-closed",
            format!(
                "session {id} is {state}; reopen requires a session closed with its closed tombstone"
            ),
        ));
    }
    for name in [
        TURN_CLAIM_FILE,
        TURN_COMPLETION_FILE,
        TERMINAL_HANDLE_FILE,
        TERMINAL_CLOSING_FILE,
    ] {
        if fs::symlink_metadata(directory.join(name)).is_ok() {
            return Err(reopen_refusal(
                "source-not-converged",
                format!("session {id} still carries {name}; its close has not converged"),
            ));
        }
    }
    let Some(text) = read_regular_text_if_present(&directory.join(REOPEN_MARKER_FILE))? else {
        return Ok(None);
    };
    let refuse = |detail: String| Err(reopen_refusal("already-reopened", detail));
    let Ok(marker) = serde_json::from_str::<ReopenMarker>(&text) else {
        return refuse(format!(
            "session {id} carries a reopen marker that cannot be read; it is treated as consumed"
        ));
    };
    let Some(new_id) = marker.reopened_by else {
        return refuse(format!("a reopen of session {id} is already in progress"));
    };
    let already = format!("session {id} was already reopened as {new_id}");
    if !valid_session_id(&new_id) {
        return refuse(format!(
            "{already}; the marker names an invalid session id, so it is treated as consumed"
        ));
    }
    let refused_directory = directory
        .parent()
        .context("session directory has no state root")?
        .join(&new_id);
    if !fs::symlink_metadata(&refused_directory).is_ok_and(|metadata| metadata.is_dir()) {
        return refuse(format!(
            "{already}; the records of {new_id} are missing, so the marker is treated as consumed"
        ));
    }
    let Some(refusal) = read_reopen_launch_refusal(&refused_directory) else {
        return refuse(already);
    };
    let cleanup = match refused_launch_cleanup(&refused_directory, &refusal.gate) {
        Ok(cleanup) => cleanup,
        Err(error) => {
            return refuse(format!(
                "{already}; that launch was refused ({}) but its records cannot be verified: {error:#}",
                refusal.gate
            ));
        }
    };
    if !cleanup.releases_marker() {
        return refuse(format!(
            "{already}; that launch was refused ({}) but {cleanup}",
            refusal.gate
        ));
    }
    Ok(Some(StaleReopenMarker {
        refused_session: new_id,
        gate: refusal.gate,
        cleanup,
    }))
}

// A recorded event that cannot be read is a turn whose outcome cannot be verified, so it
// refuses under `request-unresolved` even when no receipt points at it (legacy sessions).
// Every recorded event is read, not only those newer than the identity that is returned: an
// older unreadable event is as unverifiable as a newer one.
fn latest_provider_event_identity(
    directory: &Path,
    provider: FirstPartyCli,
    id: &str,
) -> Result<Option<(String, String)>> {
    let mut newest = None;
    for path in event_paths(directory)? {
        let event: SessionEvent = read_json(&path).map_err(|error| {
            reopen_refusal(
                "request-unresolved",
                format!(
                    "session {id} has a recorded result {} that cannot be read: {error:#}",
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default()
                ),
            )
        })?;
        if event.provider != provider.as_str() {
            continue;
        }
        if let Some(provider_session_id) = event.provider_session_id
            && !provider_session_id.trim().is_empty()
        {
            let event_id = path
                .file_name()
                .and_then(|name| name.to_str())
                .context("event path has no file name")?
                .to_owned();
            newest = Some((event_id, provider_session_id));
        }
    }
    Ok(newest)
}

fn read_resumed_from(directory: &Path) -> Result<Option<ResumedFrom>> {
    let provenance: ReopenProvenance = read_json(&directory.join("manifest.json"))?;
    Ok(provenance.resumed_from)
}

fn record_resumed_from(
    directory: &Path,
    manifest: &SessionManifest,
    resumed_from: &ResumedFrom,
) -> Result<()> {
    let mut value = serde_json::to_value(manifest)?;
    value
        .as_object_mut()
        .context("session manifest is not a JSON object")?
        .insert(
            "resumed_from".to_owned(),
            serde_json::to_value(resumed_from)?,
        );
    write_json_atomic(&directory.join("manifest.json"), &value)
}

// The winner's hold on a closed source. Dropping it before `finalize` removes the marker
// again, so a reopen that never created its session leaves the source reopenable.
#[derive(Debug)]
struct ReopenMarkerClaim {
    path: PathBuf,
    claim: String,
    finalized: bool,
}

impl ReopenMarkerClaim {
    fn finalize(mut self, new_session_id: &str) -> Result<()> {
        let _lock = lock_turn_claim(&self.path.with_file_name(TURN_CLAIM_FILE))?;
        let text = read_regular_text_if_present(&self.path)?
            .context("reopen marker disappeared before the new session was recorded")?;
        let mut marker: ReopenMarker =
            serde_json::from_str(&text).context("invalid reopen marker")?;
        if marker.claim != self.claim {
            bail!("reopen marker belongs to a different reopen attempt");
        }
        marker.reopened_by = Some(new_session_id.to_owned());
        write_json_atomic(&self.path, &marker)?;
        self.finalized = true;
        Ok(())
    }
}

impl Drop for ReopenMarkerClaim {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let Ok(_lock) = lock_turn_claim(&self.path.with_file_name(TURN_CLAIM_FILE)) else {
            return;
        };
        let current = read_regular_text_if_present(&self.path)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<ReopenMarker>(&text).ok());
        if current.is_some_and(|marker| marker.claim == self.claim) {
            let _ = remove_file_if_present(&self.path);
        }
    }
}

// Serializes concurrent reopens of one closed source under the source's own turn-claim lock:
// the closed gates are re-checked under the lock and the marker is created with
// `create_new`, so exactly one attempt can hold it.
fn claim_reopen_marker(
    directory: &Path,
    id: &str,
    provider_session_id: &str,
) -> Result<ReopenMarkerClaim> {
    let path = directory.join(REOPEN_MARKER_FILE);
    let _lock = lock_turn_claim(&directory.join(TURN_CLAIM_FILE))?;
    if let Some(stale) = verify_reopen_source_is_closed(directory, id)? {
        // The gate re-ran under the lock, so the stale marker still names a refused launch
        // whose cleanup is verified now; releasing it here is the reconciliation the
        // crashed or timed-out parent never performed.
        remove_file_if_present(&path).with_context(|| {
            format!(
                "could not release the stale reopen marker of session {id} (reopened as {}, refused at {}, {})",
                stale.refused_session, stale.gate, stale.cleanup
            )
        })?;
    }
    let claim = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        TURN_CLAIM_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let marker = ReopenMarker {
        schema: 1,
        claim: claim.clone(),
        provider_session_id: provider_session_id.to_owned(),
        reopened_by: None,
        created_unix_ms: unix_ms(),
    };
    write_private(&path, &serde_json::to_vec_pretty(&marker)?).map_err(|error| {
        reopen_refusal(
            "already-reopened",
            format!("could not claim session {id} for reopen: {error:#}"),
        )
    })?;
    Ok(ReopenMarkerClaim {
        path,
        claim,
        finalized: false,
    })
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
        Some((session, request_id)) => {
            let mut extra = serde_json::Map::new();
            if let Some(gate) = outcome.as_ref().err().and_then(reopen_refusal_gate) {
                extra.insert("gate".to_owned(), serde_json::json!(gate));
            }
            finish_request_with_extra(outcome, json, &session, &request_id, extra)
        }
        None => outcome,
    }
}

fn run_tell_inner(request: TellRequest, address: &mut Option<(String, String)>) -> Result<()> {
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    // Attached results are resolved and pinned before the target is recovered, repaired,
    // claimed, or sent to, so a failed resolution leaves every session unchanged.
    let attached = context::resolve(&request.context_results)?;
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
    let prompt = native_delegation_prompt(
        &delegation_source(),
        &attached.prompt_with_attachments(&request.prompt),
    );
    let follow_up_transport = provider::follow_up_transport(provider);
    let resumed_from = read_resumed_from(&directory)?;
    let (mut claim, baseline) =
        acquire_ready_turn_claim_with_context(&directory, &request.id, &attached.sources)?;
    let claim_token = claim.token.clone();
    let receipt = claim.receipt.clone();
    *address = Some((request.id.clone(), receipt.request_id.clone()));
    refuse_follow_up_to_shared_conversation(
        provider,
        &directory,
        &request.id,
        resumed_from.as_ref(),
        deadline,
        &mut claim,
        &previous_state,
    )?;
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
                    record_cross_session_delivery_uncertainty(&directory, &mut claim, &error);
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
            &receipt,
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
        &receipt,
        Some(&event),
    )
}

// A resumed session's conversation is re-checked immediately before every follow-up
// delivery. A refusal sends nothing: the claim is released and the reason is recorded in
// one status write, so the session stays ready with the refusal in its status and its
// receipt stays unresolved. This is the same best-effort detection the launch ran; a
// foreign resume that registers after this point is caught only by the next delivery.
#[allow(clippy::too_many_arguments)]
fn refuse_follow_up_to_shared_conversation(
    provider: FirstPartyCli,
    directory: &Path,
    id: &str,
    resumed_from: Option<&ResumedFrom>,
    deadline: Instant,
    claim: &mut TurnClaim,
    previous_state: &str,
) -> Result<()> {
    let Err(error) = verify_reopened_conversation_exclusive(
        provider,
        directory,
        resumed_from,
        deadline,
        ResumedHolderCheck::BeforeFollowUp,
    ) else {
        return Ok(());
    };
    let error = error.context(format!(
        "follow-up to reopened session {id} was refused before delivery"
    ));
    Err(record_follow_up_refusal(id, claim, previous_state, error))
}

// The claim is released and the refusal reason is published in one write under the
// lifecycle lock, and only while the claim is still this request's. Once another `tell`
// owns the turn, nothing is written: that turn keeps its claim, receipt, and status, and
// the refused request's receipt stays unresolved.
fn record_follow_up_refusal(
    id: &str,
    claim: &mut TurnClaim,
    previous_state: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    match claim.release_now_with_reason(previous_state, format!("{error:#}")) {
        Ok(true) => error,
        Ok(false) => error.context(format!(
            "the turn claim of session {id} already belonged to another request; its status was left unchanged"
        )),
        Err(release_error) => error.context(format!(
            "the turn claim of session {id} could not be released: {release_error:#}"
        )),
    }
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
    let sessions = sessions_in(&state_root()?, &request)?;
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

/// The `sessions` listing over one state root. Listing is also a lifecycle-lock holder: it
/// converges interrupted completions and closes and repairs dead owners before it reads.
fn sessions_in(root: &Path, request: &SessionsRequest) -> Result<Vec<serde_json::Value>> {
    let mut sessions = Vec::new();
    if root.is_dir() {
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
            // Repair runs completion recovery first itself, unconditionally and under the
            // same lifecycle lock, so a listing publishes every finished turn before it
            // decides on the owner without a separate recovery pass.
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
                "resumed_from": read_resumed_from(&directory).ok().flatten(),
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
    Ok(sessions)
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
    close_session_surface(&directory, &request.id, None)
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
            // An explicit close may reclaim the exact startup surface after its wrapper
            // died, including an uncertain spawn. This is not automatic failure cleanup:
            // neither a live/unknown owner nor an unbound/foreign handle gains authority.
            if let Some(launch) = launch::read(directory)?
                && launch.phase != launch::Phase::Spawned
            {
                let status: SessionStatus = read_json(&directory.join("status.json"))?;
                let owner: NativeSessionOwner = read_json(&directory.join(SESSION_OWNER_FILE))?;
                let observed = query::observe_owner_record(&owner);
                if status.state == "failed"
                    && owner.managed_session_id.as_deref() == Some(expected_session_id)
                    && (observed.process_alive == Some(false)
                        || observed.identity_matches == Some(false))
                {
                    session.verify_managed_session(expected_session_id)?;
                    return Ok(false);
                }
            }
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

#[cfg(test)]
fn close_repaired_session_state<F>(directory: &Path, close_terminal: F) -> Result<()>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
{
    close_repaired_session_state_with_reason(directory, None, close_terminal)
}

// Repairs a dead native owner first, then closes. A repair failure is the recorded close
// error; otherwise `reason` (if any) is kept in the closed status.
fn close_repaired_session_state_with_reason<F>(
    directory: &Path,
    reason: Option<String>,
    close_terminal: F,
) -> Result<()>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
{
    let repair_error = repair_dead_native_owner(directory)
        .err()
        .map(|error| format!("pre-close session repair failed: {error:#}"));
    close_session_state_with_error(directory, repair_error.or(reason), close_terminal)
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
    fault_point("claiming the terminal handle for close")?;
    match fs::rename(&terminal_path, &closing_path) {
        Ok(()) => {
            fault_point("syncing the claimed terminal handle's directory")?;
            sync_parent_directory(&closing_path)?
        }
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
    fault_point("removing a record file")?;
    match fs::remove_file(path) {
        Ok(()) => {
            fault_point("syncing a removed record's directory")?;
            sync_parent_directory(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub(super) fn rename_session_file(from: &Path, to: &Path) -> Result<()> {
    fault_point("renaming a record file")?;
    fs::rename(from, to)
        .with_context(|| format!("failed to rename {} to {}", from.display(), to.display()))?;
    fault_point("syncing a renamed record's directory")?;
    sync_parent_directory(to)?;
    if from.parent() != to.parent() {
        sync_parent_directory(from)?;
    }
    Ok(())
}

fn finish_request(outcome: Result<()>, json: bool, session: &str, request_id: &str) -> Result<()> {
    finish_request_with_extra(outcome, json, session, request_id, serde_json::Map::new())
}

fn finish_request_with_extra(
    outcome: Result<()>,
    json: bool,
    session: &str,
    request_id: &str,
    extra: serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
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
            for (key, field) in extra {
                value[key] = field;
            }
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
    receipt: &requests::Receipt,
    event: Option<&SessionEvent>,
) -> Result<()> {
    emit_session_result_with(
        json,
        id,
        terminal_session,
        provider,
        receipt,
        event,
        &serde_json::Map::new(),
    )
}

fn emit_session_result_with(
    json: bool,
    id: &str,
    terminal_session: &terminal::TerminalSession,
    provider: FirstPartyCli,
    receipt: &requests::Receipt,
    event: Option<&SessionEvent>,
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let request_id = &receipt.request_id;
    if json {
        let mut value = serde_json::json!({
                "ok": true,
                "schema_version": 1,
                "session": id,
                "request_id": request_id,
                "context_sources": receipt.context_sources,
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
        });
        for (key, field) in extra {
            value[key.as_str()] = field.clone();
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("session: {id}\nrequest: {request_id}");
        for (key, field) in extra {
            if let Some(field) = field.as_str() {
                println!("{}: {}", key, terminal_safe_text(field, false));
            }
        }
        if let Some(event) = event {
            println!();
            println!("{}", terminal_safe_text(&event.message, true));
        }
    }
    Ok(())
}

fn run_session(id: &str) -> Result<()> {
    let directory = session_directory(id)?;
    launch::log(&directory, "wrapper_started");
    if let Some(record) = launch::read(&directory)? {
        let status: SessionStatus = read_json(&directory.join("status.json"))?;
        if record.phase != launch::Phase::Pending
            || status.state != "launching"
            || current_turn_claim_token(&directory)?.as_deref() != Some(&record.claim_token)
        {
            launch::log(
                &directory,
                "wrapper_exit_code=1; launch cancelled or already attempted",
            );
            bail!(
                "provider launch cancelled or already attempted; the recorded session is unchanged"
            );
        }
    }
    let result = (|| {
        launch::restore_stdout()?;
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!("native-session must run in a visible interactive terminal");
        }
        let owner = current_native_session_owner(id)?;
        write_json_atomic(&directory.join(SESSION_OWNER_FILE), &owner)?;
        launch::log(&directory, "owner_recorded");
        run_session_inner(&directory)
    })();
    match &result {
        Ok(()) => launch::log(&directory, "wrapper_exit_code=0"),
        Err(error) => launch::log(&directory, &format!("wrapper_exit_code=1; {error:#}")),
    }
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
    if let Some(record) = launch::read(directory)?
        && record.phase != launch::Phase::Spawned
        && current_turn_claim_token(directory)?.as_deref() != Some(&record.claim_token)
    {
        return Ok(());
    }
    recover_pending_completion_locked(directory, &claim_path)?;
    let status: SessionStatus = read_json(&directory.join("status.json"))?;
    if !matches!(status.state.as_str(), "closed" | "exited" | "failed") {
        match result {
            Ok(()) => update_status(directory, "exited", Some(0), None)?,
            Err(error) => {
                let reason = if launch::uncertain(directory) {
                    format!(
                        "{error:#}; provider spawn is uncertain; the claim is retained, do not resend"
                    )
                } else {
                    format!("{error:#}")
                };
                update_status(directory, "failed", Some(1), Some(reason))?;
            }
        }
    }
    if launch::uncertain(directory) && status.state != "closed" {
        return Ok(());
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
    // A reopened session launches through the adapter's resume plan. The plan carries no
    // prompt: the initial prompt reaches the reopened process through the same transport a
    // fresh launch uses, so `prompt_is_positional` is false here by construction.
    let resumed_from = read_resumed_from(directory)?;
    let (provider_arguments, prompt_is_positional, completion_monitor, environment_removals) =
        match &resumed_from {
            Some(resumed_from) => {
                let provider::ResumePlan {
                    arguments,
                    completion_monitor,
                    environment_removals,
                } = provider::prepare_resume(
                    provider,
                    provider::ResumeContext {
                        bridge_executable: &executable,
                        directory,
                        provider_session_id: &resumed_from.provider_session_id,
                    },
                )?;
                (arguments, false, completion_monitor, environment_removals)
            }
            None => {
                let provider::LaunchPlan {
                    arguments,
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
                (
                    arguments,
                    prompt_is_positional,
                    completion_monitor,
                    environment_removals,
                )
            }
        };
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
    // The reopen ownership gate ran read-only before the source was claimed; the provider
    // grants no exclusive hold on the conversation, so it runs again here, after every other
    // preparation and immediately before the process exists. A holder that appeared in
    // between refuses under the same gate; the record carries the gate to the reopen command.
    if let Some(resumed_from) = &resumed_from
        && let Err(error) =
            provider::verify_reopen_available(provider, &resumed_from.provider_session_id)
    {
        let _ = completion_monitor.stop();
        return Err(record_reopen_refusal(
            directory,
            REOPEN_LAUNCH_GATE,
            format!("{error:#}"),
        ));
    }
    provider_command
        .current_dir(&manifest.workspace)
        .env(SESSION_DIR_ENV, directory)
        .env("AGENT_BRIDGE_NATIVE_SESSION_ID", &manifest.id)
        .env("AGENT_BRIDGE_EXECUTABLE", &executable);
    launch::provider_stderr(&mut provider_command);
    // Spawn and its durable identity are fenced against cancellation by the lifecycle
    // lock. Failed identity recording ends the child but stays delivery-uncertain: an
    // argument-delivered prompt could already have been consumed before cleanup.
    let child = launch::spawn(directory, &mut provider_command, |child| {
        record_provider_process(directory, &manifest.id, child)?;
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
        Ok(())
    });
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let _ = completion_monitor.stop();
            return Err(error).with_context(|| {
                format!(
                    "failed to start {} at {}",
                    provider.as_str(),
                    manifest.provider_path.display()
                )
            });
        }
    };
    let status = child.wait();
    if let Ok(status) = &status {
        launch::log(
            directory,
            &format!("provider_exit_code={:?}", status.code()),
        );
    }
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
    commit_provider_completion_within_locked(
        directory,
        claim_path,
        claim_token,
        event,
        status_error,
        status_state,
        EVENT_READ_LIMIT,
    )
}

/// [`commit_provider_completion_with_status_locked`] with an explicit event size limit, so
/// tests exercise the size policy without writing 64 MiB records. Production callers pass
/// [`EVENT_READ_LIMIT`]: the journal is created under the same limit the publication
/// predicate reads with, so no interruption can change whether a completion recovers.
fn commit_provider_completion_within_locked(
    directory: &Path,
    claim_path: &Path,
    claim_token: &str,
    event: SessionEvent,
    status_error: Option<String>,
    status_state: &str,
    event_limit: u64,
) -> Result<()> {
    let mut pending =
        PendingTurnCompletion::new_with_status(claim_token, event, status_error, status_state)?;
    // Request indexing must not prevent a provider-verified completion from publishing.
    // A missing or damaged receipt remains explicitly unresolved in request queries.
    if let Ok(Some(receipt)) = requests::for_claim(directory, claim_token) {
        pending.event_file = receipt.event_file;
    }
    let pending = bound_pending_completion(pending, event_limit)?;
    // Every caller recovers under the lifecycle lock first, so a journal that still exists
    // here belongs to a completion that could not be recovered; refuse to replace it.
    let completion_path = directory.join(TURN_COMPLETION_FILE);
    if completion_path.exists() {
        bail!("a pending native turn completion is still awaiting recovery")
    }
    // The journal is published by rename so that a partial journal never exists at its
    // final path; the temporary file carries the same private permissions.
    write_json_atomic(&completion_path, &pending)?;
    recover_pending_completion_locked(directory, claim_path)?;
    Ok(())
}

/// The one size policy, applied where a completion is journaled. An event record larger
/// than `event_limit` (the bytes the journal would write) is never journaled as
/// publishable: the publication predicate reads at most that many bytes, so such a record
/// could be published when the commit stopped before writing it and refused when it
/// stopped after. The completion is journaled instead as a failure whose error names the
/// size, keeping the provider identity, so every interruption settles to the same state.
fn bound_pending_completion(
    mut pending: PendingTurnCompletion,
    event_limit: u64,
) -> Result<PendingTurnCompletion> {
    let size = serde_json::to_vec_pretty(&pending.event)?.len() as u64;
    if size <= event_limit {
        return Ok(pending);
    }
    let error =
        format!("provider result of {size} bytes exceeds the {event_limit} byte event limit");
    pending.event.message = String::new();
    pending.event.error = Some(error.clone());
    pending.status_error = Some(error);
    pending.status_state = "failed".to_owned();
    Ok(pending)
}

fn read_regular_text_if_present(path: &Path) -> Result<Option<String>> {
    Ok(read_regular_bytes_if_present(path)?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

/// The raw bytes of a regular session file, so callers that must not alter a record can
/// decode it strictly instead of through the lossy snapshot reader.
fn read_regular_bytes_if_present(path: &Path) -> Result<Option<Vec<u8>>> {
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
    fs::read(path)
        .map(Some)
        .with_context(|| format!("failed to read {}", path.display()))
}

fn create_session(spec: SessionSpec) -> Result<CreatedSession> {
    create_session_in(&state_root()?, spec)
}

fn create_session_in(root: &Path, spec: SessionSpec) -> Result<CreatedSession> {
    create_session_within(root, &home_directories(), spec)
}

/// [`create_session_in`] with the directories whose own entries are taken as durable
/// (the user's home directory in production), so the state-root ancestry walk stops
/// there instead of at the filesystem root.
fn create_session_within(
    root: &Path,
    durable_directories: &[PathBuf],
    spec: SessionSpec,
) -> Result<CreatedSession> {
    let ancestry_error = create_state_root(root, durable_directories)?;
    let temp = tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(root)?;
    let directory = temp.keep();
    set_private_directory_permissions(&directory)?;
    // The session directory is itself a record: sync the root so its entry survives a
    // crash the same way the files written inside it do.
    sync_directory(root)
        .with_context(|| format!("failed to sync state root {}", root.display()))?;
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("session directory name is not UTF-8")?
        .to_owned();
    require_valid_session_id(&id)?;
    let events = directory.join("events");
    fs::create_dir(&events)?;
    set_private_directory_permissions(&events)?;
    sync_directory(&directory)
        .with_context(|| format!("failed to sync session directory {}", directory.display()))?;
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
    // An ancestry sync failure never blocks the session: the root lacks its receipt, so
    // the next creation repeats the walk, and the launch status records what failed.
    update_status(&directory, "launching", None, ancestry_error)?;
    Ok(CreatedSession {
        id,
        directory,
        manifest,
    })
}

/// The receipt stored at `STATE_ROOT_DURABLE_FILE`. Only its existence as a regular file
/// carries meaning; the fields describe the walk that wrote it.
#[derive(Serialize)]
struct StateRootDurabilityReceipt {
    schema: u32,
    synced_unix_ms: u128,
}

/// Creates the state root and every missing ancestor, then makes the root's ancestry
/// durable unless the durability receipt already proves it is. A directory entry is a
/// record like the files inside it: the session directory is only durable once the root's
/// entry is, and the root's entry is only durable once every ancestor's entry is.
///
/// The walk does not depend on who created the directories. A creator that made the root
/// or an ancestor and stopped before the parent-directory syncs leaves an existing root
/// whose ancestry is not durable, and a creator that probed while another was still
/// creating sees only part of what the other made. So every creation that finds no
/// receipt syncs the entry of the root and of each ancestor above it, nearest first, up to
/// and including the entry that sits directly in the filesystem root or in one of
/// `durable_directories`, whichever comes first, bounded by
/// `STATE_ROOT_ANCESTRY_SYNC_LIMIT` entries. Concurrent creators may both walk; the syncs
/// are idempotent. The receipt is written through [`write_json_atomic`], which also syncs
/// the root, only after the walk succeeded.
///
/// Returns the walk's failure, if any, for the session's launch status: a missing receipt
/// never blocks creation, and the receipt stays absent so the next creation walks again.
fn create_state_root(root: &Path, durable_directories: &[PathBuf]) -> Result<Option<String>> {
    let mut created = Vec::new();
    let mut probe = root;
    loop {
        match fs::symlink_metadata(probe) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                created.push(probe.to_path_buf());
                match probe.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => probe = parent,
                    _ => break,
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect state directory {}", probe.display())
                });
            }
        }
    }
    for directory in created.iter().rev() {
        // A concurrent creator may win the race; the ancestry walk below covers its
        // entries and this creator's alike.
        if let Err(error) = fs::create_dir(directory)
            && !directory.is_dir()
        {
            return Err(error).with_context(|| {
                format!("failed to create state directory {}", directory.display())
            });
        }
    }
    set_private_directory_permissions(root)?;
    if state_root_durability_receipt_present(root) {
        return Ok(None);
    }
    fault_point("syncing the state root's ancestry")?;
    if let Err(error) = sync_state_root_ancestry(root, durable_directories) {
        return Ok(Some(format!(
            "state root ancestry was not made durable: {error:#}"
        )));
    }
    let receipt = StateRootDurabilityReceipt {
        schema: 1,
        synced_unix_ms: unix_ms(),
    };
    write_json_atomic(&root.join(STATE_ROOT_DURABLE_FILE), &receipt)?;
    Ok(None)
}

/// Whether the state root carries its durability receipt. Only a regular file counts; a
/// missing, unreadable, or non-regular entry means the ancestry walk runs again, which is
/// harmless when the ancestry was in fact durable.
fn state_root_durability_receipt_present(root: &Path) -> bool {
    fs::symlink_metadata(root.join(STATE_ROOT_DURABLE_FILE))
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

/// Syncs the directory that holds the entry of `root`, then the one that holds its
/// parent's entry, and so on. The walk stops after the sync that makes durable an entry
/// sitting directly in the filesystem root or in one of `durable_directories`, whose own
/// entries are not the bridge's to establish, or after `STATE_ROOT_ANCESTRY_SYNC_LIMIT`
/// entries.
fn sync_state_root_ancestry(root: &Path, durable_directories: &[PathBuf]) -> Result<()> {
    let mut entry = root;
    for _ in 0..STATE_ROOT_ANCESTRY_SYNC_LIMIT {
        let Some(parent) = entry.parent() else {
            break;
        };
        let holder = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        sync_directory(holder)
            .with_context(|| format!("failed to sync state directory {}", holder.display()))?;
        let holder_is_filesystem_root = parent.as_os_str().is_empty() || parent.parent().is_none();
        if holder_is_filesystem_root
            || durable_directories
                .iter()
                .any(|durable| same_directory(parent, durable))
        {
            break;
        }
        entry = parent;
    }
    Ok(())
}

/// Whether two paths name the same directory, by spelling or after canonicalisation.
fn same_directory(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (left.canonicalize(), right.canonicalize()),
            (Ok(left), Ok(right)) if left == right
        )
}

/// The directories whose own entries the state-root ancestry walk takes as durable: the
/// user's home directory under either of the variables `default_state_root` reads.
fn home_directories() -> Vec<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .collect()
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
    session_directory_in(&state_root()?, id)
}

fn session_directory_in(root: &Path, id: &str) -> Result<PathBuf> {
    require_valid_session_id(id)?;
    let directory = root.join(id);
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
    // The manifest decides a session's scope (workspace, provider) for every read-only
    // query, so it is read like every other session record: a link at `manifest.json`
    // would let content outside the state root steer a search or an attach, and is
    // refused rather than followed.
    let path = directory.join("manifest.json");
    let bytes = read_regular_bytes_if_present(&path)?
        .with_context(|| format!("failed to read {}", path.display()))?;
    let manifest: SessionManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;
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

// Durable record writes. Every helper below syncs the file it changed and then the
// directory that holds its entry, so a crash after the helper returns cannot lose the
// record on a POSIX file system that honours fsync. See README "권한과 세션 경계" for the
// classification of which records go through these helpers and the platform limits.
//
// Under `cfg(test)` three thread-local hooks observe these helpers: `fault_point` refuses
// the next filesystem mutation once an injected budget is spent, which models a process
// that died between two mutations, `record_sync` logs every sync call in order, and
// `sync_directory` refuses the directories named by `with_sync_failure`, which models a
// sync the operating system rejects. All are inert outside tests.

#[cfg(test)]
thread_local! {
    static FAULT_BUDGET: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static SYNC_LOG: std::cell::RefCell<Option<Vec<SyncRecord>>> =
        const { std::cell::RefCell::new(None) };
    /// Directories whose sync fails with an injected error while `with_sync_failure` runs.
    static SYNC_FAILURES: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Every journaled event the publication predicate opened, in order, so a test can
    /// prove when a search reads a journaled event and when it does not read it at all.
    static PUBLICATION_READ_LOG: std::cell::RefCell<Option<Vec<PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum SyncRecord {
    File(PathBuf),
    Directory(PathBuf),
}

/// Refuses the step that follows it once the injected fault budget reaches zero. A budget of
/// `k` lets exactly `k` boundaries pass and then fails every later one, like a process that
/// stopped there. Boundaries sit before each record mutation (temporary-file creation,
/// permission and content writes, rename, removal) and between a rename or removal and the
/// sync that makes it durable; a fault after a temporary file exists leaves it behind.
fn fault_point(label: &str) -> Result<()> {
    #[cfg(test)]
    {
        FAULT_BUDGET.with(|budget| match budget.get() {
            None => Ok(()),
            Some(0) => bail!("injected fault before {label}"),
            Some(remaining) => {
                budget.set(Some(remaining - 1));
                Ok(())
            }
        })
    }
    #[cfg(not(test))]
    {
        let _ = label;
        Ok(())
    }
}

#[cfg(test)]
fn injected_fault(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("injected fault before")
}

#[cfg(test)]
fn with_fault_budget<T>(budget: usize, run: impl FnOnce() -> T) -> T {
    FAULT_BUDGET.with(|cell| cell.set(Some(budget)));
    let outcome = run();
    FAULT_BUDGET.with(|cell| cell.set(None));
    outcome
}

#[cfg(test)]
fn with_sync_log<T>(run: impl FnOnce() -> T) -> (T, Vec<SyncRecord>) {
    SYNC_LOG.with(|log| *log.borrow_mut() = Some(Vec::new()));
    let outcome = run();
    let records = SYNC_LOG.with(|log| log.borrow_mut().take().unwrap_or_default());
    (outcome, records)
}

/// Runs `run` while every sync of `directory` on this thread fails with an injected
/// error. The sync is still logged first, so a test sees that it was attempted.
#[cfg(test)]
fn with_sync_failure<T>(directory: &Path, run: impl FnOnce() -> T) -> T {
    SYNC_FAILURES.with(|failures| failures.borrow_mut().push(directory.to_path_buf()));
    let outcome = run();
    SYNC_FAILURES.with(|failures| failures.borrow_mut().clear());
    outcome
}

#[cfg(test)]
fn injected_sync_failure(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("injected sync failure for")
}

/// Runs `run` and returns, in order, the path of every journaled event the publication
/// predicate opened on this thread while it ran.
#[cfg(test)]
fn with_publication_read_log<T>(run: impl FnOnce() -> T) -> (T, Vec<PathBuf>) {
    PUBLICATION_READ_LOG.with(|log| *log.borrow_mut() = Some(Vec::new()));
    let outcome = run();
    let paths = PUBLICATION_READ_LOG.with(|log| log.borrow_mut().take().unwrap_or_default());
    (outcome, paths)
}

fn record_publication_read(path: &Path) {
    #[cfg(test)]
    PUBLICATION_READ_LOG.with(|log| {
        if let Some(log) = log.borrow_mut().as_mut() {
            log.push(path.to_path_buf());
        }
    });
    #[cfg(not(test))]
    let _ = path;
}

#[derive(Clone, Copy)]
enum SyncKind {
    File,
    Directory,
}

fn record_sync(kind: SyncKind, path: &Path) {
    #[cfg(test)]
    SYNC_LOG.with(|log| {
        if let Some(log) = log.borrow_mut().as_mut() {
            log.push(match kind {
                SyncKind::File => SyncRecord::File(path.to_path_buf()),
                SyncKind::Directory => SyncRecord::Directory(path.to_path_buf()),
            });
        }
    });
    #[cfg(not(test))]
    {
        let _ = (kind, path);
    }
}

fn sync_file(file: &File, path: &Path) -> Result<()> {
    record_sync(SyncKind::File, path);
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("JSON path has no parent")?;
    fault_point("creating a temporary record file")?;
    let temporary = tempfile::Builder::new()
        .prefix(".agent-bridge-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    let mut temporary = fault_point_keeping_temporary(
        "writing a temporary record's permissions and content",
        temporary,
    )?;
    set_private_file_permissions(temporary.as_file())?;
    temporary.write_all(&serde_json::to_vec_pretty(value)?)?;
    temporary.flush()?;
    sync_file(temporary.as_file(), temporary.path())?;
    let temporary = fault_point_keeping_temporary(
        "renaming a temporary record over its final path",
        temporary,
    )?;
    let persisted = persist_record(temporary, path)
        .with_context(|| format!("failed to persist {}", path.display()))?;
    fault_point("syncing a renamed record")?;
    sync_file(&persisted, path)?;
    sync_parent_directory(path)?;
    Ok(())
}

// How long a record replacement waits for another handle to the record to close.
#[cfg(windows)]
const RECORD_REPLACE_WAIT: Duration = Duration::from_secs(1);

/// Renames a temporary record over its final path. Windows refuses to replace a file
/// while any other handle to it is open, whatever that handle shares, and a caller that
/// polls a record holds one for a moment on every read (2026-10-01: a launcher's read of
/// `launch.json` failed the wrapper's replacement of it with access denied). The rename
/// is therefore repeated for a bounded time on those two errors; a record that stays
/// open, or cannot be replaced for another reason, still fails.
fn persist_record(temporary: tempfile::NamedTempFile, path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};
        let deadline = Instant::now() + RECORD_REPLACE_WAIT;
        let mut temporary = temporary;
        loop {
            match temporary.persist(path) {
                Ok(file) => return Ok(file),
                Err(error)
                    if Instant::now() < deadline
                        && error.error.raw_os_error().is_some_and(|code| {
                            [ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION].contains(&(code as u32))
                        }) =>
                {
                    temporary = error.file;
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.error),
            }
        }
    }
    #[cfg(not(windows))]
    {
        temporary.persist(path).map_err(|error| error.error)
    }
}

/// A fault at a boundary after the temporary file exists leaves that file behind, exactly
/// as an abrupt stop would; ordinary errors still remove it when the handle drops.
fn fault_point_keeping_temporary(
    label: &str,
    temporary: tempfile::NamedTempFile,
) -> Result<tempfile::NamedTempFile> {
    match fault_point(label) {
        Ok(()) => Ok(temporary),
        Err(error) => {
            #[cfg(test)]
            {
                let _ = temporary.keep();
            }
            Err(error)
        }
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    fault_point("creating a private record file")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    // The file now exists at its final path; a fault here leaves it empty, as a stop would.
    fault_point("writing a private record's permissions and content")?;
    set_private_file_permissions(&file)?;
    file.write_all(bytes)?;
    file.flush()?;
    fault_point("syncing a private record")?;
    sync_file(&file, path)?;
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

fn sync_directory(directory: &Path) -> Result<()> {
    record_sync(SyncKind::Directory, directory);
    #[cfg(test)]
    if SYNC_FAILURES.with(|failures| failures.borrow().iter().any(|failed| failed == directory)) {
        bail!("injected sync failure for {}", directory.display());
    }
    sync_directory_entries(directory)
}

#[cfg(unix)]
fn sync_directory_entries(directory: &Path) -> Result<()> {
    File::open(directory)?.sync_all()?;
    Ok(())
}

// Windows flushes the directory's metadata through a handle opened with backup semantics.
// NTFS journals directory entries, so this is a best-effort flush of the volume's cached
// metadata rather than the POSIX guarantee that the entry itself reached stable storage.
#[cfg(windows)]
fn sync_directory_entries(directory: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)?
        .sync_all()?;
    Ok(())
}

// No supported transport exists on other targets; the directory entry is left to the
// operating system's own write-back and the records are not claimed durable there.
#[cfg(not(any(unix, windows)))]
fn sync_directory_entries(_directory: &Path) -> Result<()> {
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
    // The tombstone is the close's commit point: every later write, whatever state it
    // asks for, restores the tombstone unchanged and does not advance the generation.
    if let Some(closed) = read_status_if_present(&closed_path)? {
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

/// The session status transition contract. A same-state write is always allowed (it
/// refreshes the timestamp or error and still takes a new generation); every other write
/// must appear in this table or `update_status` rejects it without advancing the
/// generation. `exited`, `failed`, and `closed` are terminal except that the first two may
/// still be closed; `closed` accepts nothing else. The one exception to the generation
/// increment is the `closed.json` tombstone: once it exists, `update_status` no longer
/// consults this table and rewrites `status.json` as a copy of the tombstone, so the
/// tombstone's generation, timestamp, and error are preserved rather than advanced. The
/// README section "권한과 세션 경계" carries the same table for operators.
///
/// | From                    | To                                                 |
/// | ----------------------- | -------------------------------------------------- |
/// | `launching`             | `running`, `awaiting-initial-input`, `failed`, `closed` |
/// | `awaiting-initial-input`| `working`, `exited`, `failed`, `closed`            |
/// | `running`               | `ready`, `exited`, `failed`, `closed`              |
/// | `ready`                 | `claimed`, `exited`, `failed`, `closed`            |
/// | `claimed`               | `working`, `ready`, `exited`, `failed`, `closed`   |
/// | `working`               | `ready`, `exited`, `failed`, `closed`              |
/// | `resume-pending`        | `working`, `ready`, `exited`, `failed`, `closed`   |
/// | `exited`, `failed`      | `closed`                                           |
/// | `closed`                | (none)                                             |
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

    // Releases the claim now and publishes `state` with `reason` in the same status write,
    // under the lifecycle lock and only while the claim file still holds this token. Returns
    // whether that write happened; a claim that another request already owns is left alone
    // together with the status it published. Dropping the claim later does nothing more.
    fn release_now_with_reason(&mut self, state: &str, reason: String) -> Result<bool> {
        if self.retained {
            return Ok(false);
        }
        self.retained = true;
        rollback_turn_claim_token_with_error(&self.path, &self.token, state, Some(reason))
    }

    fn retain(mut self) {
        self.retain_in_place();
    }
}

/// Compare-and-set status write on behalf of one turn: the status changes only while the
/// claim named by `claim_token` is still the installed claim, checked and written under
/// the turn-claim lifecycle lock. A writer whose turn has already been released or
/// replaced is rejected with `Ok(false)` and leaves the status generation untouched.
///
/// Every status writer that reports about a specific turn (delivery failures, delivery
/// uncertainty) goes through this helper; writers that report about the session as a
/// whole (process exit, monitor failure, close) use `update_status` under their own
/// guards.
fn update_status_for_turn(
    directory: &Path,
    claim_token: &str,
    state: &str,
    error: Option<String>,
) -> Result<bool> {
    let claim_path = directory.join(TURN_CLAIM_FILE);
    let _lock = lock_turn_claim(&claim_path)?;
    update_status_for_turn_locked(directory, claim_token, state, error)
}

fn update_status_for_turn_locked(
    directory: &Path,
    claim_token: &str,
    state: &str,
    error: Option<String>,
) -> Result<bool> {
    if current_turn_claim_token(directory)?.as_deref() != Some(claim_token) {
        return Ok(false);
    }
    update_status(directory, state, None, error)?;
    Ok(true)
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
        let _ = update_status_for_turn(directory, &claim.token, "working", Some(error));
    } else {
        let _ = update_status(directory, "failed", None, Some(error));
    }
}

// The send can outlive the turn: the target may complete the delivered turn and a later
// tell may claim the session before this sender learns that its paste timed out. The
// failure then belongs to a released turn and must not touch the current turn's status.
fn record_follow_up_terminal_delivery_failure(
    directory: &Path,
    claim: &mut TurnClaim,
    failure: &terminal::TerminalSendFailure,
) {
    if failure.delivery_may_have_occurred() {
        claim.retain_in_place();
        let error = terminal_safe_text(&format!("{:#}", failure.error()), true);
        let _ = update_status_for_turn(directory, &claim.token, "working", Some(error));
    }
}

// A turn that stays claimed looks like ordinary work from the state alone, so the status
// keeps the reason until the target completes the turn or the session is closed. The target
// can complete a delivered turn before its sender stops settling; the session status then
// belongs to whichever turn holds the claim now, not to this report. The initial messenger
// is no exception: on Windows the initial turn can complete and a later `tell` can install
// a replacement claim before the initial messenger reports its uncertainty.
fn record_cross_session_delivery_uncertainty(
    directory: &Path,
    claim: &mut TurnClaim,
    error: &anyhow::Error,
) {
    claim.retain_in_place();
    let error = terminal_safe_text(&format!("{error:#}"), true);
    let _ = update_status_for_turn(directory, &claim.token, "working", Some(error));
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
    rollback_turn_claim_token_with_error(path, expected_token, state, None).map(|_| ())
}

// Under the lifecycle lock: the claim is removed and the status is rolled back to `state`
// (carrying `error`) only while the claim file still holds `expected_token`. Returns
// whether that happened. The reliability branch introduces `update_status_for_turn` for
// claim-checked status writes; this helper is the equivalent for the rollback path.
//
// Merge reconciliation note: this must remain one lifecycle critical section that does the
// ownership check, the status publication, and the claim removal under a single hold of the
// lock. It is not a drop-in for `update_status_for_turn` followed by removal: that helper
// takes the same lock (calling it from inside this section would lock recursively), and
// calling the claim-checking helper after the removal would find no claim and report the
// write as not owned. Keep all three steps here, under the one lock.
fn rollback_turn_claim_token_with_error(
    path: &Path,
    expected_token: &str,
    state: &str,
    error: Option<String>,
) -> Result<bool> {
    let _lock = lock_turn_claim(path)?;
    let current = match fs::read_to_string(path) {
        Ok(current) => current,
        Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(read_error) => return Err(read_error).context("failed to inspect native turn claim"),
    };
    if current.trim() != expected_token {
        return Ok(false);
    }
    remove_turn_claim_locked(path)?;
    let directory = path
        .parent()
        .context("turn claim has no session directory")?;
    update_status(directory, state, None, error)?;
    Ok(true)
}

#[cfg(test)]
fn acquire_turn_claim(directory: &Path) -> Result<TurnClaim> {
    acquire_turn_claim_with_context(directory, &[])
}

// The receipt records the pinned context sources the caller already resolved.
fn acquire_turn_claim_with_context(
    directory: &Path,
    context_sources: &[requests::ContextSource],
) -> Result<TurnClaim> {
    let path = directory.join(TURN_CLAIM_FILE);
    let _lock = lock_turn_claim(&path)?;
    create_turn_claim_locked(path, context_sources)
}

#[cfg(test)]
fn acquire_ready_turn_claim(directory: &Path, session_id: &str) -> Result<(TurnClaim, usize)> {
    acquire_ready_turn_claim_after_claim(directory, session_id, || Ok(()))
}

fn acquire_ready_turn_claim_with_context(
    directory: &Path,
    session_id: &str,
    context_sources: &[requests::ContextSource],
) -> Result<(TurnClaim, usize)> {
    acquire_ready_turn_claim_with_callbacks(
        directory,
        session_id,
        || Ok(()),
        || {},
        context_sources,
    )
}

#[cfg(test)]
fn acquire_ready_turn_claim_after_claim<F>(
    directory: &Path,
    session_id: &str,
    after_claim: F,
) -> Result<(TurnClaim, usize)>
where
    F: FnOnce() -> Result<()>,
{
    acquire_ready_turn_claim_with_callbacks(directory, session_id, after_claim, || {}, &[])
}

fn acquire_ready_turn_claim_with_callbacks<F, G>(
    directory: &Path,
    session_id: &str,
    after_claim: F,
    before_publish: G,
    context_sources: &[requests::ContextSource],
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
        create_turn_claim_locked(path.clone(), context_sources)?
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

fn create_turn_claim_locked(
    path: PathBuf,
    context_sources: &[requests::ContextSource],
) -> Result<TurnClaim> {
    fault_point("creating the turn claim")?;
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
    sync_file(&file, &path)?;
    sync_parent_directory(&path)?;
    let directory = path
        .parent()
        .context("turn claim has no session directory")?;
    let receipt = match requests::create(directory, &token, context_sources) {
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

/// Converges every partial lifecycle transition that a stopped process can leave behind,
/// under the turn-claim lifecycle lock. Returns whether any record changed.
///
/// Two transitions are journaled and therefore recoverable: a provider completion (journal
/// -> event -> status -> claim release -> journal removal) and an explicit or repair close
/// (tombstone -> status -> set aside an unverified event -> claim release -> journal
/// removal). The `closed.json` tombstone is the durable commit point of a close: once it
/// exists the session is closed even when the later cleanup steps never ran, so recovery
/// finishes those steps instead of publishing. Both sequences release the claim before they
/// remove the journal: while the claim is installed the journal is the only evidence that
/// the event at its path is the provider's committed result, so no interruption may leave
/// the claim without the journal. For the same reason both sequences sync `events/` before
/// they discard the journal of an event that already matches it: the completion that wrote
/// the event may have stopped between its rename and that sync.
fn recover_pending_completion_locked(directory: &Path, claim_path: &Path) -> Result<bool> {
    let completion_path = directory.join(TURN_COMPLETION_FILE);
    if let Some(tombstone) = read_status_if_present(&directory.join(CLOSED_STATUS_FILE))? {
        return converge_interrupted_close_locked(directory, claim_path, &tombstone);
    }
    let Some(text) = read_regular_text_if_present(&completion_path)? else {
        return Ok(false);
    };
    let pending: PendingTurnCompletion =
        serde_json::from_str(&text).context("invalid pending native turn completion")?;
    validate_pending_completion(&pending)?;

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
            // The same byte-match predicate every other lifecycle path uses: a semantically
            // equal event stored in another encoding is not the journal's committed write.
            match journaled_event_state(directory, &pending)? {
                JournaledEventState::Committed => (),
                JournaledEventState::Absent => {
                    bail!("claim-free pending completion has no committed event")
                }
                JournaledEventState::Mismatched | JournaledEventState::Oversized(_) => {
                    bail!("claim-free pending completion event does not match its journal")
                }
            }
            let status: SessionStatus = read_json(&directory.join("status.json"))?;
            if status.state != pending.status_state || status.error != pending.status_error {
                bail!("claim-free pending completion has no matching terminal status")
            }
            // The claim is released only after the event's directory was synced, but the
            // journal is the last evidence of the result, so its removal is preceded by
            // the same barrier regardless of which run released the claim.
            sync_committed_event_directory(directory)?;
            remove_file_if_present(&completion_path)?;
            Ok(true)
        }
        Err(error) => Err(error).context("failed to inspect pending completion turn claim"),
    }
}

// Finishes a close whose tombstone was written but whose later cleanup steps did not run.
// The tombstone is preserved unchanged: status.json is rewritten from it (update_status
// copies the tombstone whenever one exists), and the turn claim, the journal, and the
// legacy resume markers are removed in the same order the uninterrupted close uses. A
// journaled completion is settled exactly as that close settles it: an event it already
// wrote stays published when it matches the journal, is set aside when it does not, and a
// journal without an event is discarded.
fn converge_interrupted_close_locked(
    directory: &Path,
    claim_path: &Path,
    tombstone: &SessionStatus,
) -> Result<bool> {
    let mut changed = false;
    let status_path = directory.join("status.json");
    let status_matches = read_status_if_present(&status_path)?.is_some_and(|status| {
        status.state == tombstone.state
            && status.generation == tombstone.generation
            && status.error == tombstone.error
    });
    if !status_matches {
        update_status(directory, "closed", None, tombstone.error.clone())?;
        changed = true;
    }
    let completion_path = directory.join(TURN_COMPLETION_FILE);
    if completion_path.exists() {
        set_aside_unverified_completion_event_for_close(directory)?;
        changed = true;
    }
    if claim_path.exists() {
        remove_turn_claim_locked(claim_path)?;
        changed = true;
    }
    remove_file_if_present(&completion_path)?;
    for name in [LEGACY_RESUME_PENDING_FILE, LEGACY_RESUME_RUNNING_FILE] {
        let path = directory.join(name);
        if path.exists() {
            remove_file_if_present(&path)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Largest event the publication predicate compares with its journal. It bounds only that
/// comparison: a larger journaled event is never read for a verdict and never published,
/// and the size policy at journal creation keeps new completions under it. It is the byte
/// budget of a whole `search`, so a search can afford at most one such comparison. It
/// does not bound the read of an ordinary, non-journaled event by `result`, `inspect`, or
/// `--context-result`; only a search bounds those reads, with its byte budget.
const EVENT_READ_LIMIT: u64 = 64 * 1024 * 1024;

/// How the file at a journal's event path relates to the journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JournaledEventState {
    /// No event file exists at the journal's event path: nothing was published.
    Absent,
    /// The event file holds exactly the bytes the journal would write, so it is the
    /// provider result under its receipt's immutable event name. Byte equality alone does
    /// not make it durable: the completion that wrote it may have stopped between the
    /// rename and the sync of `events/`, so every lifecycle path syncs `events/` before it
    /// discards the journal of a committed event (`sync_committed_event_directory`).
    Committed,
    /// A different record occupies the journal's event path.
    Mismatched,
    /// The file at the journal's event path is larger than the caller's read limit (its
    /// size in bytes), so it was not compared and is never treated as published.
    Oversized(u64),
}

/// The publication predicate's verdict together with what it cost: the bytes it read, and
/// the stored text when they are the journal's, so a budgeted caller can charge the read
/// once and search the record without reading it again.
struct JournaledEventRead {
    state: JournaledEventState,
    bytes_read: u64,
    committed_text: Option<String>,
}

/// The single byte-match predicate: a journaled event is committed exactly when its file
/// holds the bytes the journal would write. Every lifecycle path (completion recovery,
/// claim-free recovery, close, interrupted close) and every read-only query decide
/// publication with this comparison.
fn journaled_event_state(
    directory: &Path,
    pending: &PendingTurnCompletion,
) -> Result<JournaledEventState> {
    Ok(journaled_event_state_within(directory, pending, EVENT_READ_LIMIT)?.state)
}

/// [`journaled_event_state`] that reads at most `limit + 1` bytes of the event. The size
/// is checked before the file is opened, and a file that grows under the read is still
/// reported as oversized: the read is cut after `limit + 1` bytes, and that one extra byte
/// is what detects the overflow, so a caller charging a byte budget may see one byte more
/// than `limit` in `bytes_read`. The `events` directory is validated before anything under
/// it is opened, so a link planted there is never followed by a publication read.
fn journaled_event_state_within(
    directory: &Path,
    pending: &PendingTurnCompletion,
    limit: u64,
) -> Result<JournaledEventRead> {
    use std::io::Read as _;
    require_events_directory(directory)?;
    let path = directory.join("events").join(&pending.event_file);
    let outcome = |state, bytes_read, committed_text| JournaledEventRead {
        state,
        bytes_read,
        committed_text,
    };
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(outcome(JournaledEventState::Absent, 0, None));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular session file: {}", path.display());
    }
    if metadata.len() > limit {
        return Ok(outcome(
            JournaledEventState::Oversized(metadata.len()),
            0,
            None,
        ));
    }
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(outcome(JournaledEventState::Absent, 0, None));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    record_publication_read(&path);
    let mut stored = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut stored)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let bytes_read = stored.len() as u64;
    if bytes_read > limit {
        return Ok(outcome(
            JournaledEventState::Oversized(bytes_read),
            bytes_read,
            None,
        ));
    }
    Ok(if stored == serde_json::to_vec_pretty(&pending.event)? {
        // The journal's bytes are canonical JSON, so the stored text is valid UTF-8.
        let text = String::from_utf8_lossy(&stored).into_owned();
        outcome(JournaledEventState::Committed, bytes_read, Some(text))
    } else {
        outcome(JournaledEventState::Mismatched, bytes_read, None)
    })
}

/// What stands at a session's `events` path when it is safe to say anything about it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventsDirectory {
    /// A real directory inside the session directory.
    Present,
    /// Nothing at all: the session holds no event record.
    Missing,
}

/// Inspects a session's `events` path with `symlink_metadata`, so a symlink or Windows
/// junction is rejected rather than followed, as is a non-directory file or an unreadable
/// entry. A missing path is reported, not rejected: readers and lifecycle steps decide
/// what an absent directory means for them.
fn events_directory_state(directory: &Path) -> Result<EventsDirectory> {
    let events = directory.join("events");
    match fs::symlink_metadata(&events) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("events directory is a symlink")
        }
        Ok(metadata) if !metadata.is_dir() => bail!("events is not a directory"),
        Ok(_) => Ok(EventsDirectory::Present),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(EventsDirectory::Missing),
        Err(error) => bail!("events directory is unreadable: {error}"),
    }
}

/// A session's `events` must be a real directory inside the session directory before any
/// record under it is opened: the shared event listing turns a missing or non-directory
/// `events` path into an empty list, a search must not present that damage as "no
/// results", and a link planted there would carry a publication read outside the state
/// root. Built on [`events_directory_state`], so a link is rejected rather than followed.
fn require_events_directory(directory: &Path) -> Result<()> {
    match events_directory_state(directory)? {
        EventsDirectory::Present => Ok(()),
        EventsDirectory::Missing => bail!("events directory is missing"),
    }
}

/// The first settlement step of a close that finds a completion journal in place. The
/// tombstone is the close's commit point, but an event the interrupted completion already
/// wrote is the provider's authoritative result: when it matches the journal byte for byte
/// it stays published (the receipt already maps the request to it), and when it does not
/// match, or is too large to compare, it is moved aside under an `unpublished-` name that
/// no query reads. A journal whose event was never written needs no step here and is
/// discarded when the close removes the journal; a session whose `events` directory is
/// missing altogether holds no event to verify and settles the same way, so a close is
/// never left permanently unsettled by that damage. A link or a non-directory at `events`
/// is still rejected, and the journal then stays in place with the claim. The move is
/// idempotent, so an interrupted close converges on the next run.
///
/// The close removes the journal only after this step and after the turn claim is
/// released: while the claim is installed, the journal is the evidence that the event at
/// its path is the committed result, so an interruption before claim release would
/// otherwise hide a published result until the next recovery. The journal is also the
/// only evidence that a set-aside event was unverified, so the move is made durable
/// before the journal can be discarded: the rename syncs `events/` itself, and a run that
/// finds the event already moved aside by an interrupted close, which may have stopped
/// between the rename and that sync, syncs `events/` again before it returns. A committed
/// event gets the same barrier: the completion that wrote it may have stopped between its
/// rename and the sync of `events/`, so the close syncs the directory before the journal,
/// the only proof that the entry is the result, is removed.
fn set_aside_unverified_completion_event_for_close(directory: &Path) -> Result<()> {
    let completion_path = directory.join(TURN_COMPLETION_FILE);
    let Some(text) = read_regular_text_if_present(&completion_path)? else {
        return Ok(());
    };
    if events_directory_state(directory)? == EventsDirectory::Missing {
        return Ok(());
    }
    let Ok(pending) = serde_json::from_str::<PendingTurnCompletion>(&text) else {
        return Ok(());
    };
    if validate_pending_completion(&pending).is_err() {
        return Ok(());
    }
    let events = directory.join("events");
    let set_aside = events.join(format!("{UNPUBLISHED_EVENT_PREFIX}{}", pending.event_file));
    match journaled_event_state(directory, &pending)? {
        JournaledEventState::Mismatched | JournaledEventState::Oversized(_) => {
            rename_session_file(&events.join(&pending.event_file), &set_aside).context(
                "failed to set aside a completion event that disagrees with its journal",
            )?;
        }
        JournaledEventState::Absent => {
            if fs::symlink_metadata(&set_aside).is_ok() {
                fault_point("syncing a set-aside completion event's directory")?;
                sync_directory(&events).with_context(|| {
                    format!("failed to sync state directory {}", events.display())
                })?;
            }
        }
        JournaledEventState::Committed => sync_committed_event_directory(directory)?,
    }
    Ok(())
}

/// Makes a committed event's directory entry durable before the journal that proves the
/// event is the provider's result can be discarded. A completion that stopped between the
/// event's rename and the sync of `events/` leaves the entry unsynced while the journal
/// still exists; every lifecycle path that discards the journal of a committed event
/// (completion recovery, claim-free recovery, close, interrupted close) syncs `events/`
/// first, so a later crash cannot lose the event together with its evidence. The sync is
/// idempotent when the completion already made the entry durable, and it is a fault
/// boundary like every other sync that follows a rename.
fn sync_committed_event_directory(directory: &Path) -> Result<()> {
    fault_point("syncing a committed completion event's directory")?;
    let events = directory.join("events");
    sync_directory(&events)
        .with_context(|| format!("failed to sync state directory {}", events.display()))
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
    match journaled_event_state(directory, pending)? {
        // The completion that wrote this event may have stopped between its rename and
        // the sync of `events/`; the journal is discarded once this returns, so the
        // entry is made durable here.
        JournaledEventState::Committed => sync_committed_event_directory(directory),
        JournaledEventState::Mismatched => {
            bail!("pending native completion event file contains different data")
        }
        JournaledEventState::Oversized(size) => {
            bail!(
                "pending native completion event file is {size} bytes, over the {EVENT_READ_LIMIT} byte read limit"
            )
        }
        JournaledEventState::Absent => {
            // The same size policy the journal was created under, re-checked for a
            // journal an earlier version wrote: an event the predicate could never compare
            // is not written, so the absent and present orders settle the same way.
            let size = serde_json::to_vec_pretty(&pending.event)?.len() as u64;
            if size > EVENT_READ_LIMIT {
                bail!(
                    "pending native completion event is {size} bytes, over the {EVENT_READ_LIMIT} byte read limit"
                )
            }
            write_json_atomic(
                &directory.join("events").join(&pending.event_file),
                &pending.event,
            )
        }
    }
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
        // An event the journal disagrees with is set aside first, then the claim is
        // released, and only then is the journal removed: every interruption of this order
        // leaves a state in which a committed event stays published and an unverified one
        // stays hidden. The journal is kept whenever an earlier step failed.
        let claim_result = set_aside_unverified_completion_event_for_close(directory)
            .and_then(|()| remove_turn_claim_locked(claim_path));
        let pending_result = if claim_result.is_ok() {
            remove_file_if_present(&directory.join(TURN_COMPLETION_FILE))
        } else {
            Ok(())
        }
        .and_then(|()| remove_file_if_present(&directory.join(LEGACY_RESUME_PENDING_FILE)));
        (
            pending_result,
            remove_file_if_present(&directory.join(LEGACY_RESUME_RUNNING_FILE)),
            claim_result,
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
    // Completion recovery runs first so a live owner's finished turn is published before
    // anything else is decided. Its failure is damage (a missing `events/`, a journal
    // whose event cannot be compared), not a reason to leave a dead owner's session
    // installed forever: the owner check still runs, a dead owner's session is closed as
    // it would be without the damage (the close settles the journal without publishing),
    // and the damage is reported in the close error. Under a live owner, or when the
    // owner cannot be shown dead, the damage is the result.
    let recovery_damage = recover_pending_completion(directory).err();
    let untouched = |damage: Option<anyhow::Error>| match damage {
        Some(error) => Err(error),
        None => Ok(false),
    };
    if launch::repair(directory)? {
        return untouched(recovery_damage);
    }
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
        return untouched(recovery_damage);
    }
    let owner_path = directory.join(SESSION_OWNER_FILE);
    let owner = match fs::read_to_string(&owner_path) {
        Ok(text) => serde_json::from_str::<NativeSessionOwner>(&text)
            .with_context(|| format!("invalid JSON in {}", owner_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return untouched(recovery_damage);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", owner_path.display()));
        }
    };
    #[cfg(windows)]
    match &owner.windows_process_identity {
        Some(identity) => {
            if terminal::verify_windows_process_identity(owner.pid, identity).is_ok() {
                return untouched(recovery_damage);
            }
        }
        // Pre-identity (v0.0.2) Windows owner records carry only a PID. Their identity is
        // unknown, not dead: while the PID is alive the session is left alone and inspect
        // reports `identity_matches: null`; only a dead PID lets repair proceed.
        None => {
            if process_is_alive(owner.pid) {
                return untouched(recovery_damage);
            }
        }
    }
    #[cfg(target_os = "macos")]
    if mac_native_owner_is_live(&owner)? {
        return untouched(recovery_damage);
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    if process_is_alive(owner.pid) {
        return untouched(recovery_damage);
    }
    let mut repair_error = status
        .error
        .clone()
        .unwrap_or_else(|| format!("native session process {} is no longer running", owner.pid));
    if let Some(damage) = &recovery_damage {
        repair_error = format!("{repair_error}; completion recovery failed: {damage:#}");
    }
    let repair_error = Some(repair_error);
    #[cfg(windows)]
    {
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
        mark_session_closed(directory, repair_error)?;
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
    provider::apply_environment_removals(
        &mut command,
        provider::probe_environment_removals(provider),
    );
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
    let log = shell_quote(state_root.join(id).join(launch::LOG).as_os_str());
    let location = format!("cd {} &&", shell_quote(workspace.as_os_str()));
    let invocation = format!(
        "{STATE_DIR_ENV}={} {} native-session {}",
        shell_quote(state_root.as_os_str()),
        shell_quote(executable.as_os_str()),
        shell_quote(OsString::from(id).as_os_str())
    );
    let start = format!("{location} {invocation}");
    let logged_start = format!(
        "{location} {}=3 {}=4 {invocation}",
        launch::STDERR_ENV,
        launch::STDOUT_ENV
    );
    Ok(format!(
        "if (umask 077; : >> {log}) 2>/dev/null; then {{ {logged_start}; bridge_status=$?; printf 'wrapper_exit_code=%s\\n' \"$bridge_status\" >> {log}; }} 3>&2 4>&1 >> {log} 2>&1; else {start}; bridge_status=$?; fi; exit \"$bridge_status\""
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
    let log = powershell_quote(state_root.join(id).join(launch::LOG).as_os_str());
    // The console launch passes this to PowerShell as one double-quoted `-Command`
    // argument and refuses a command that holds a double quote, so every string here is
    // single-quoted.
    Ok(format!(
        "$bridgeStatus = 1; try {{ Set-Location -LiteralPath {} -ErrorAction Stop; $env:{} = {}; & {} native-session {}; $bridgeStatus = $LASTEXITCODE }} catch {{ try {{ Add-Content -LiteralPath {log} -Value $_ -ErrorAction Stop }} catch {{}} }}; try {{ Add-Content -LiteralPath {log} -Value ('wrapper_exit_code=' + $bridgeStatus) -ErrorAction Stop }} catch {{}}; exit $bridgeStatus",
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
