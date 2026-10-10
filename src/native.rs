use agent_bridge::PUBLIC_COMMAND;
#[cfg(test)]
mod tests;

mod cancel;
mod consent;
mod context;
mod doctor;
mod hold;
mod provider;
mod provider_process;
mod query;
mod reopen;
use reopen::{
    REOPEN_LAUNCH_GATE, RecordedReopenRefusal, ResumedFrom, ResumedHolderCheck,
    close_surface_after_reopen_verification_failure, read_resumed_from, record_reopen_refusal,
    refuse_follow_up_to_shared_conversation, reopen_refusal_gate, run_reopen,
    verify_reopened_conversation_exclusive,
};
mod self_test;
mod session;

use session::turn::*;
use session::*;
mod settings;
mod terminal;

#[cfg(all(target_os = "macos", test))]
use terminal::macos::apple_terminal::{
    apple_terminal_startup_absent_with, record_legacy_terminal_app, terminal_app_ancestor,
    terminal_app_process_with,
};
#[cfg(all(target_os = "macos", test))]
use terminal::macos::apple_terminal::{terminal_app_process, terminal_surface_absent};
#[cfg(all(target_os = "macos", test))]
use terminal::macos::process::{macos_process_info, macos_process_start, macos_processes_named};
#[cfg(all(target_os = "macos", test))]
use terminal::macos::warp::{prepare_warp_close, terminate_owned_foreground_group};
use terminal::ownership::NativeSessionOwner;
#[cfg(test)]
use terminal::ownership::verify_terminal_owner_attestation;
#[cfg(windows)]
use terminal::ownership::verify_terminal_surface_ownership;
#[cfg(all(target_os = "macos", test))]
use terminal::ownership::{MacTerminalAppIdentity, surface_outlives_owner};
#[cfg(test)]
use terminal::ownership::{
    MacTerminalShellIdentity, NativeProcessIdentity, native_owner_identity_matches,
    verified_terminal_owner_process_group, verified_terminal_shell_process_group,
};
#[cfg(all(any(target_os = "macos", windows), test))]
use terminal::ownership::{
    TerminalCloseAuthority, verify_terminal_close_authority,
    verify_terminal_close_authority_with_observations,
};
#[cfg(all(windows, test))]
use terminal::ownership::{current_native_session_owner, verified_windows_native_owner};
#[cfg(windows)]
use terminal::{windows_console_handle_path, windows_console_root_never_ran};

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
static TURN_CLAIM_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) enum NativeCommand {
    Consent(Vec<String>),
    Settings(Vec<String>),
    Ask(AskRequest),
    SelfTest(self_test::Request),
    Tell(TellRequest),
    Hold(hold::Request),
    Cancel(cancel::Request),
    Reopen(ReopenRequest),
    Inspect {
        id: String,
        json: bool,
        timeline: bool,
        request: Option<String>,
    },
    Result(query::ResultRequest),
    Wait(query::wait::WaitRequest),
    Search(query::SearchRequest),
    Status(query::status::StatusRequest),
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
    Iterm2Host {
        directory: PathBuf,
    },
    AppleTerminalHost {
        directory: PathBuf,
    },
    WezTermHost {
        directory: PathBuf,
    },
    GhosttyHost,
    WarpHost {
        directory: PathBuf,
        attempt: String,
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SessionEvent {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    cancelled: bool,
    provider: String,
    message: String,
    #[serde(default)]
    error: Option<String>,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    #[serde(default)]
    created_unix_ms: Option<u128>,
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
            | "hold"
            | "cancel"
            | "reopen"
            | "sessions"
            | "status"
            | "inspect"
            | "result"
            | "wait"
            | "search"
            | "doctor"
            | "prune-sessions"
            | "close-session"
            | "native-session"
            | "native-hook"
            | "native-provider-control"
            | "native-console-control"
            | "native-console-host"
            | "native-iterm2-host"
            | "native-terminal-host"
            | "native-wezterm-host"
            | "native-ghostty-host"
            | "native-warp-host"
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
        "hold" => hold::parse(rest),
        "cancel" => cancel::parse(rest),
        "consent" => Ok(NativeCommand::Consent(rest.to_vec())),
        "settings" => Ok(NativeCommand::Settings(rest.to_vec())),
        "reopen" => parse_reopen(rest),
        "inspect" => query::parse_inspect(rest),
        "result" => query::parse_result(rest),
        "wait" => query::wait::parse(rest),
        "search" => query::parse_search(rest),
        "doctor" => doctor::parse_args(rest),
        "sessions" => parse_sessions(rest),
        "status" => query::status::parse(rest),
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
        "native-console-host"
        | "native-iterm2-host"
        | "native-wezterm-host"
        | "native-terminal-host" => {
            // The tab's process does not necessarily inherit the state root, so it is
            // given the session directory itself.
            let directory = PathBuf::from(one_positional(
                rest,
                &format!("{command} requires one session directory"),
            )?);
            let id = directory
                .file_name()
                .and_then(|name| name.to_str())
                .with_context(|| format!("{command} requires a session directory"))?;
            require_valid_session_id(id)?;
            if !directory.is_absolute() {
                bail!("{command} requires an absolute session directory");
            }
            match command.as_str() {
                "native-iterm2-host" => Ok(NativeCommand::Iterm2Host { directory }),
                "native-terminal-host" => Ok(NativeCommand::AppleTerminalHost { directory }),
                "native-wezterm-host" => Ok(NativeCommand::WezTermHost { directory }),
                _ => Ok(NativeCommand::ConsoleHost { directory }),
            }
        }
        "native-ghostty-host" => {
            // The surface is created before its session is bound to it, so this host
            // is given nothing: its launch reaches it through its own terminal.
            if !rest.is_empty() {
                bail!("native-ghostty-host takes no argument");
            }
            Ok(NativeCommand::GhosttyHost)
        }
        "native-warp-host" => {
            let [directory, attempt] = rest else {
                bail!("native-warp-host requires a session directory and attempt token");
            };
            let directory = PathBuf::from(directory);
            let id = directory
                .file_name()
                .and_then(|name| name.to_str())
                .context("native-warp-host requires a session directory")?;
            require_valid_session_id(id)?;
            if !directory.is_absolute() {
                bail!("native-warp-host requires an absolute session directory");
            }
            if attempt.len() != 32 || !attempt.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("native-warp-host requires a valid attempt token");
            }
            Ok(NativeCommand::WarpHost {
                directory,
                attempt: attempt.clone(),
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
        NativeCommand::Hold(request) => hold::run(request),
        NativeCommand::Cancel(request) => cancel::run(request),
        NativeCommand::Reopen(request) => run_reopen(request),
        NativeCommand::Inspect {
            id,
            json,
            timeline,
            request,
        } => query::run_inspect(&id, json, timeline, request.as_deref()),
        NativeCommand::Result(request) => query::run_result(request),
        NativeCommand::Wait(request) => query::wait::run(request),
        NativeCommand::Search(request) => query::run_search(request),
        NativeCommand::Doctor(request) => doctor::run(request),
        NativeCommand::Sessions(request) => run_sessions(request),
        NativeCommand::Status(request) => query::status::run(request),
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
        NativeCommand::Iterm2Host { directory } => terminal::iterm2_host(&directory),
        NativeCommand::AppleTerminalHost { directory } => terminal::apple_terminal_host(&directory),
        NativeCommand::WezTermHost { directory } => terminal::wezterm_host(&directory),
        NativeCommand::GhosttyHost => terminal::ghostty_host(),
        NativeCommand::WarpHost { directory, attempt } => terminal::warp_host(&directory, &attempt),
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
    let directory = Reader::session_directory(id)?;
    let manifest = Reader::open_unchecked(&directory).manifest()?;
    let session: terminal::TerminalSession =
        RecordReader::at(&windows_console_handle_path(&directory, action)).json()?;
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
    let input_path = input_name.map(|name| {
        Reader::open_unchecked(&directory)
            .private(name)
            .path()
            .to_owned()
    });
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
    let mut initial_claim =
        turn::claim(&Store::open_unchecked(&created.directory), context_sources)?;
    let expected_claim_token = initial_claim.token().to_owned();
    let receipt = initial_claim.receipt().clone();
    *address = Some((created.id.clone(), receipt.request_id.clone()));
    let launch_deadline = launch::begin(
        &Store::open_unchecked(&created.directory),
        &expected_claim_token,
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
    initial_claim.retain_in_place();
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
            let error =
                match launch::terminal_failed(&Store::open_unchecked(&created.directory), &error) {
                    Ok(()) => error,
                    Err(record_error) => {
                        error.context(format!("terminal launch failure handoff: {record_error:#}"))
                    }
                };
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
    launch::wait(
        &Store::open_unchecked(&created.directory),
        &terminal_session,
        launch_deadline,
    )?;
    // A trust response is separate from model input. Only verified consent and an
    // adapter-recognized exact workspace dialog may produce one guarded response.
    consent::complete_launch(&created.directory, provider, &terminal_session, deadline)?;
    // The existing delivery paths own rollback/uncertainty after confirmed startup.
    initial_claim.rollback_on_drop();
    let initial_prompt_transport = provider::initial_prompt_transport(provider);
    let mut expected_turn_id = None;
    if initial_prompt_transport == provider::InitialPromptTransport::TerminalPasteAfterLaunch {
        let mut delivery_may_have_occurred = false;
        let delivery = (|| -> Result<()> {
            wait_for_status(
                &created.directory,
                SessionState::AwaitingInitialInput,
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
            let initial_prompt = Reader::open_unchecked(&created.directory)
                .initial_prompt()
                .context("failed to read the preserved initial prompt")?;
            let initial_prompt =
                provider::terminal_initial_prompt(provider, &created.directory, &initial_prompt)?;
            let mut prompt_file = tempfile::Builder::new()
                .prefix("pending-prompt-")
                .suffix(".txt")
                .tempfile_in(&created.directory)?;
            RecordStore::set_file_private(prompt_file.as_file())?;
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
            initial_claim.begin_delivery()?;
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
            initial_claim.complete_initial_delivery()?;
            Ok(())
        })();
        if let Err(error) = delivery {
            let _ = initial_claim.settle_delivery(if delivery_may_have_occurred {
                turn::Delivery::Uncertain(&error)
            } else {
                turn::Delivery::NotSent(&error)
            });
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
        let mut delivery_may_have_occurred = false;
        let delivery = (|| -> Result<String> {
            wait_for_status(
                &created.directory,
                SessionState::AwaitingInitialInput,
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
            let prompt = Reader::open_unchecked(&created.directory)
                .initial_prompt()
                .context("failed to read the preserved initial prompt")?;
            let bridge_executable =
                std::env::current_exe().context("failed to locate the current executable")?;
            remaining_turn_timeout(deadline, timeout)?;
            verify_reopened_conversation_exclusive(
                provider,
                &created.directory,
                resumed_from.as_ref(),
                deadline,
                ResumedHolderCheck::BeforeInitialDelivery,
            )?;
            initial_claim.begin_delivery()?;
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
                    delivery_may_have_occurred = true;
                    initial_claim.complete_initial_delivery()?;
                    Ok(request_id)
                }
                Err(failure) if failure.delivery_may_have_occurred() => {
                    delivery_may_have_occurred = true;
                    let error = failure.into_error();
                    Err(error).context(
                        "Claude initial cross-session delivery could not be confirmed; the turn remains claimed until completion or explicit close",
                    )
                }
                Err(failure) => {
                    let error = failure.into_error();
                    Err(error).context("Claude initial cross-session delivery was not sent")
                }
            }
        })();
        match delivery {
            Ok(request_id) => expected_turn_id = Some(request_id),
            Err(error) => {
                let _ = initial_claim.settle_delivery(if delivery_may_have_occurred {
                    turn::Delivery::Uncertain(&error)
                } else {
                    turn::Delivery::NotSent(&error)
                });
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
        initial_claim.settle_delivery(turn::Delivery::Sent)?;
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

    let event = turn::wait(
        &Store::open_unchecked(&created.directory),
        0,
        expected_turn_id.as_deref(),
        Some(&expected_claim_token),
        deadline,
        timeout,
    )
    .with_context(|| {
        format!(
            "session {} in {} did not return a successful result; use `{PUBLIC_COMMAND} inspect {}` to inspect its recorded state",
            created.id,
            terminal_session.kind.display_name(),
            created.id
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
    Store::open_unchecked(directory)
        .write_provider_process(&ProviderProcessRecord {
            schema: 1,
            managed_session_id: managed_session_id.to_owned(),
            pid,
            windows_process_identity,
            spawned_unix_ms: unix_ms(),
        })
        .context("failed to record the spawned provider process")
}

// Closes a managed session's own visible surface exactly as an explicit `close-session`
// does, through the same terminal-close authority checks. `reason` is kept in the closed
// status when the close itself reports nothing, so a session closed because of a detected
// conflict still says why.
fn close_session_surface(directory: &Path, id: &str, reason: Option<String>) -> Result<()> {
    session::close::close(&Store::open_unchecked(directory), reason, |session| {
        terminal::ownership::close_owned_surface(directory, id, session)
    })
    .map(|_| ())
}

fn verify_terminal_surface_ownership_until(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    deadline: Instant,
    requested: Duration,
) -> Result<()> {
    let timeout = remaining_turn_timeout(deadline, requested)?;
    terminal::ownership::verify_terminal_surface_ownership_with_timeout(
        directory,
        expected_session_id,
        session,
        Some(timeout),
    )
}

fn run_tell(request: TellRequest) -> Result<()> {
    let json = request.json;
    let session = request.id.clone();
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
        None => {
            if json
                && let Err(error) = &outcome
                && error.is::<session::hold::Refusal>()
            {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "schema_version": 1, "ok": false, "session": session, "error": format!("{error:#}")
                    }))?
                );
            }
            outcome
        }
    }
}

fn run_tell_inner(request: TellRequest, address: &mut Option<(String, String)>) -> Result<()> {
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    // Attached results are resolved and pinned before the target is recovered, repaired,
    // claimed, or sent to, so a failed resolution leaves every session unchanged.
    let attached = context::resolve(&request.context_results)?;
    let directory = Reader::session_directory(&request.id)?;
    session::hold::permit(&Reader::open_unchecked(&directory), &request.id)?;
    Store::open_unchecked(&directory).converge()?;
    let manifest = Reader::open_unchecked(&directory).manifest()?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    let terminal_session: terminal::TerminalSession =
        Reader::open_unchecked(&directory).terminal()?;
    verify_terminal_surface_ownership_until(
        &directory,
        &request.id,
        &terminal_session,
        deadline,
        request.timeout,
    )?;
    let previous_state =
        session::hold::follow_up_admission(&Reader::open_unchecked(&directory), &request.id)?;
    let prompt = native_delegation_prompt(
        &delegation_source(),
        &attached.prompt_with_attachments(&request.prompt),
    );
    let follow_up_transport = provider::follow_up_transport(provider);
    let resumed_from = read_resumed_from(&directory)?;
    let (mut claim, baseline) = turn::claim_ready(
        &Store::open_unchecked(&directory),
        &request.id,
        &attached.sources,
    )?;
    let claim_token = claim.token().to_owned();
    let receipt = claim.receipt().clone();
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
        | provider::FollowUpTransport::ProviderQueue => {
            let prepared = (|| -> Result<_> {
                remaining_turn_timeout(deadline, request.timeout)?;
                let provider_turn_id = if follow_up_transport
                    == provider::FollowUpTransport::ProviderCrossSessionMessage
                {
                    Some(provider::new_cross_session_turn_id(provider)?)
                } else {
                    None
                };
                let bridge_executable =
                    std::env::current_exe().context("failed to locate the current executable")?;
                claim.begin_delivery()?;
                Ok((provider_turn_id, bridge_executable))
            })();
            let (provider_turn_id, bridge_executable) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    let _ = claim.settle_delivery(turn::Delivery::NotSent(&error));
                    return Err(error);
                }
            };
            let correlation_id = provider_turn_id.as_deref().unwrap_or(&claim_token);
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
                Err(failure) if failure.delivery_may_have_occurred() => {
                    let error = failure.into_error();
                    let _ = claim.settle_delivery(turn::Delivery::Uncertain(&error));
                    return Err(error).with_context(|| {
                        format!(
                            "provider follow-up transport {} could not confirm delivery; the turn remains claimed until the target reports completion or the session is explicitly closed",
                            follow_up_transport.as_str()
                        )
                    });
                }
                Err(failure) => {
                    let error = failure.into_error();
                    let _ = claim.settle_delivery(turn::Delivery::NotSent(&error));
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
    claim.settle_delivery(turn::Delivery::Sent)?;

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
    let event = turn::wait(
        &Store::open_unchecked(&directory),
        baseline,
        expected_turn_id.as_deref(),
        Some(&claim_token),
        deadline,
        request.timeout,
    )
    .with_context(|| {
        format!(
            "session {} in {} did not return a successful result; use `{PUBLIC_COMMAND} inspect {}` to inspect its recorded state",
            request.id,
            terminal_session.kind.display_name(),
            request.id
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

#[allow(clippy::too_many_arguments)]
fn deliver_terminal_follow_up(
    provider: FirstPartyCli,
    follow_up_transport: provider::FollowUpTransport,
    directory: &Path,
    session_id: &str,
    terminal_session: &terminal::TerminalSession,
    prompt: &str,
    claim_token: &str,
    claim: &mut turn::Claim,
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
        RecordStore::set_file_private(prompt_file.as_file())?;
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
            let _ = claim.settle_delivery(turn::Delivery::NotSent(&error));
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
    if let Err(error) = claim.begin_delivery() {
        let _ = provider::cancel_terminal_follow_up(provider, directory, claim_token);
        let _ = claim.settle_delivery(turn::Delivery::NotSent(&error));
        return Err(error);
    }
    let send_timeout = match remaining_turn_timeout(deadline, requested_timeout) {
        Ok(timeout) => timeout,
        Err(error) => {
            let _ = provider::cancel_terminal_follow_up(provider, directory, claim_token);
            let _ = claim.settle_delivery(turn::Delivery::NotSent(&error));
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
        let _ = claim.settle_delivery(turn::Delivery::NotSent(&error));
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
        let _ = claim.settle_delivery(if failure.delivery_may_have_occurred() {
            turn::Delivery::Uncertain(failure.error())
        } else {
            turn::Delivery::NotSent(failure.error())
        });
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
    // The child ran until the deadline and was ended there. Only the macOS AppleScript
    // runner asks; the other platforms carry the fact without reading it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    timed_out: bool,
}

impl CommandOutputFailure {
    fn not_started(error: anyhow::Error) -> Self {
        Self {
            error,
            process_started: false,
            timed_out: false,
        }
    }

    fn started(error: anyhow::Error) -> Self {
        Self {
            error,
            process_started: true,
            timed_out: false,
        }
    }

    fn ended_at_deadline(mut self) -> Self {
        self.timed_out = true;
        self
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn timed_out(&self) -> bool {
        self.timed_out
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
    command_output_with_stdin_until_classified(command, Stdio::null(), deadline, label)
}

fn command_output_with_stdin_until_classified(
    command: &mut Command,
    stdin: Stdio,
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
        .stdin(stdin)
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
                return Err(
                    CommandOutputFailure::started(anyhow::anyhow!("{label} timed out"))
                        .ended_at_deadline(),
                );
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
    let sessions = sessions_in(&Reader::state_root()?, &request)?;
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
    sessions_query(root, request, true)
}

// Ownership discovery must not recover, repair, or otherwise change shared records.
fn sessions_in_read_only(root: &Path, request: &SessionsRequest) -> Result<Vec<serde_json::Value>> {
    sessions_query(root, request, false)
}

fn sessions_query(
    root: &Path,
    request: &SessionsRequest,
    repair: bool,
) -> Result<Vec<serde_json::Value>> {
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
            let Ok(manifest) = Reader::open_unchecked(&directory).manifest() else {
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
            if repair {
                let _ = session::close::repair_dead_owner(&Store::open_unchecked(&directory));
            }
            let status = Reader::open_unchecked(&directory).status().ok();
            let state = status
                .as_ref()
                .map(|value| value.state.as_str())
                .unwrap_or("unknown");
            if request.state.as_ref().is_some_and(|filter| filter != state) {
                continue;
            }
            let terminal = Reader::open_unchecked(&directory).terminal().ok();
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
                "results": Reader::open_unchecked(&directory).events().map(|paths| paths.len()).unwrap_or(0),
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
        prune_closed_sessions(&Reader::state_root()?, now_unix_ms - retention_ms)?
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
        if !RecordReader::at(
            Reader::open_unchecked(&directory)
                .record(CoreRecord::Manifest)
                .path(),
        )
        .is_regular_file()?
        {
            continue;
        }
        let Ok(manifest) = Reader::open_unchecked(&directory).manifest() else {
            continue;
        };
        if manifest.id != id {
            continue;
        }
        let closed = match Reader::open_unchecked(&directory).regular_closed_if_present() {
            Ok(Some(closed)) => closed,
            Ok(None) | Err(_) => continue,
        };
        let status = match Reader::open_unchecked(&directory).regular_status_if_present() {
            Ok(Some(status)) => status,
            Ok(None) | Err(_) => continue,
        };
        if closed.state != SessionState::Closed
            || status.state != SessionState::Closed
            || closed.updated_unix_ms > cutoff_unix_ms
            || status.updated_unix_ms > cutoff_unix_ms
            || Reader::open_unchecked(&directory).has_active_session_capability()
            || Reader::open_unchecked(&directory).native_owner_blocks_prune()?
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
        session::RecordStore::at(&directory)
            .remove_directory_all()
            .with_context(|| format!("failed to prune closed session {id}"))?;
        removed.push(id);
    }
    removed.sort();
    Ok(removed)
}

fn run_close(request: CloseRequest) -> Result<()> {
    confirm_explicit_close(request.explicit)?;
    let directory = Reader::session_directory(&request.id)?;
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
            let mut value = Reader::session_directory(session)
                .and_then(|directory| {
                    query::request_result(&Reader::open_unchecked(&directory), request_id)
                })
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
            "session {session}, request {request_id}; inspect with `{PUBLIC_COMMAND} result {session} --request {request_id} --json`; this error alone is not proof of non-delivery; inspect the recorded outcome before deciding whether to retry"
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
    let (elapsed, elapsed_reason) = query::observed_elapsed(Some(receipt), event);
    if json {
        let mut value = serde_json::json!({
                "ok": true,
                "schema_version": 1,
                "session": id,
                "request_id": request_id,
                "context_sources": receipt.context_sources,
                "bridge_observed_elapsed_ms": elapsed,
                "bridge_observed_elapsed_reason": elapsed_reason,
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
    let directory = Reader::session_directory(id)?;
    launch::log(&Store::open_unchecked(&directory), "wrapper_started");
    if let Some(record) = launch::read(&Reader::open_unchecked(&directory))? {
        let status: SessionStatus = Reader::open_unchecked(&directory).status()?;
        if record.phase != launch::Phase::Pending
            || status.state != SessionState::Launching
            || turn::current_claim_token(&Reader::open_unchecked(&directory))?.as_deref()
                != Some(&record.claim_token)
        {
            launch::log(
                &Store::open_unchecked(&directory),
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
        let owner = terminal::ownership::current_surface_owner(&directory, id)?;
        Store::open_unchecked(&directory).write_owner(&owner)?;
        launch::log(&Store::open_unchecked(&directory), "owner_recorded");
        run_session_inner(&directory)
    })();
    match &result {
        Ok(()) => launch::log(&Store::open_unchecked(&directory), "wrapper_exit_code=0"),
        Err(error) => launch::log(
            &Store::open_unchecked(&directory),
            &format!("wrapper_exit_code=1; {error:#}"),
        ),
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
    let claim_path = Reader::open_unchecked(directory)
        .record(CoreRecord::TurnClaim)
        .path()
        .to_owned();
    let _claim_lock = Store::open_unchecked((claim_path).with_file_name("")).lock()?;
    if let Some(record) = launch::read(&Reader::open_unchecked(directory))?
        && record.phase != launch::Phase::Spawned
        && turn::current_claim_token(&Reader::open_unchecked(directory))?.as_deref()
            != Some(&record.claim_token)
    {
        return Ok(());
    }
    recover_pending_completion_locked(directory, &claim_path)?;
    let status: SessionStatus = Reader::open_unchecked(directory).status()?;
    if !matches!(
        status.state,
        SessionState::Closed | SessionState::Exited | SessionState::Failed
    ) {
        match result {
            Ok(()) => update_status(directory, SessionState::Exited, Some(0), None)?,
            Err(error) => {
                let reason = if launch::uncertain(&Reader::open_unchecked(directory)) {
                    format!(
                        "{error:#}; provider spawn is uncertain; the claim is retained, do not resend"
                    )
                } else {
                    format!("{error:#}")
                };
                update_status(directory, SessionState::Failed, Some(1), Some(reason))?;
            }
        }
    }
    if launch::uncertain(&Reader::open_unchecked(directory)) && status.state != SessionState::Closed
    {
        return Ok(());
    }
    remove_turn_claim_locked(&claim_path)
}

fn run_session_inner(directory: &Path) -> Result<()> {
    let manifest = Reader::open_unchecked(directory).manifest()?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    check_provider_version(provider, &manifest.provider_path)?;
    let prompt_path = Reader::open_unchecked(directory)
        .record(CoreRecord::InitialPrompt)
        .path()
        .to_owned();
    let prompt = Reader::open_unchecked(directory)
        .initial_prompt()
        .context("failed to read initial prompt")?;
    let initial_prompt_transport = provider::initial_prompt_transport(provider);

    let executable = std::env::current_exe().context("failed to locate the current executable")?;
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
    let child = launch::spawn(
        &Store::open_unchecked(directory),
        &mut provider_command,
        |child| {
            record_provider_process(directory, &manifest.id, child)?;
            match initial_prompt_transport {
                provider::InitialPromptTransport::ProviderArgument => {
                    session::RecordStore::at(&prompt_path)
                        .remove_raw()
                        .context("failed to remove the accepted initial prompt")?;
                    update_status(directory, SessionState::Running, None, None)?;
                }
                provider::InitialPromptTransport::ProviderCrossSessionMessageAfterLaunch
                | provider::InitialPromptTransport::TerminalPasteAfterLaunch => {
                    update_status(directory, SessionState::AwaitingInitialInput, None, None)?;
                }
            }
            Ok(())
        },
    );
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
            &Store::open_unchecked(directory),
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
    Reader::open_unchecked(&directory).validate_hook_directory()?;
    let manifest = Reader::open_unchecked(&directory).manifest()?;
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

fn create_session(spec: SessionSpec) -> Result<CreatedSession> {
    create_session_in(&Reader::state_root()?, spec)
}

fn create_session_in(root: &Path, spec: SessionSpec) -> Result<CreatedSession> {
    create_session_within(root, &Reader::home_directories(), spec)
}

/// [`create_session_in`] with the directories whose own entries are taken as durable
/// (the user's home directory in production), so the state-root ancestry walk stops
/// there instead of at the filesystem root.
fn create_session_within(
    root: &Path,
    durable_directories: &[PathBuf],
    spec: SessionSpec,
) -> Result<CreatedSession> {
    let (directory, id, ancestry_error) = Store::create_directory(root, durable_directories)?;
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
    Store::open_unchecked(&directory).write_manifest(&manifest)?;
    Store::open_unchecked(&directory)
        .record(CoreRecord::InitialPrompt)
        .write_private(spec.prompt.as_bytes())?;
    // An ancestry sync failure never blocks the session: the root lacks its receipt, so
    // the next creation repeats the walk, and the launch status records what failed.
    update_status(&directory, SessionState::Launching, None, ancestry_error)?;
    Ok(CreatedSession {
        id,
        directory,
        manifest,
    })
}

#[cfg(test)]
fn valid_status_transition(current: &SessionState, next: &SessionState) -> bool {
    current.clone().transition_allowed(next.clone())
}

/// Largest event the publication predicate compares with its journal. It bounds only that
/// comparison: a larger journaled event is never read for a verdict and never published,
/// and the size policy at journal creation keeps new completions under it. It is the byte
/// budget of a whole `search`, so a search can afford at most one such comparison. It
/// does not bound the read of an ordinary, non-journaled event by `result`, `inspect`, or
/// `--context-result`; only a search bounds those reads, with its byte budget.
const EVENT_READ_LIMIT: u64 = 64 * 1024 * 1024;

// macOS repair deliberately retains a surface that may outlive its owner. That
// cleanup obligation must not turn a known process exit into a request timeout.
// This only diagnoses a dead PID: a reused/live PID or an unknown identity is not
// proof of death, and nothing here consumes the surface or the pending claim.
fn unix_ms() -> u128 {
    #[cfg(test)]
    if let Some(now) = session::tests::FIXED_UNIX_MS.get() {
        return now;
    }
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

fn delegation_source() -> String {
    std::env::var("AGENT_BRIDGE_NATIVE_SESSION_ID").unwrap_or_else(|_| "external".to_owned())
}

// The first line of every prompt that Bridge sends to a provider. It is framing, not an
// identity: an adapter can tell by it a prompt that Bridge sent from a text that a
// provider put around the same prompt, and nothing more.
const NATIVE_DELEGATION_HEADER: &str = "[Agent Bridge native delegation]";

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
        "{NATIVE_DELEGATION_HEADER}\nSource: {}\n\n{}",
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
