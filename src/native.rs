use crate::native::session::close::budget::CLOSE_TIMEOUT;
use crate::native::session::primitives::{
    SESSION_DIR_ENV, delegation_source, require_valid_session_id, unix_ms,
};
use crate::native::session::schema::{SESSION_SCHEMA, SessionEvent, SessionManifest};
use agent_bridge::PUBLIC_COMMAND;
#[cfg(test)]
use terminal::ownership::NativeSessionOwner;
#[cfg(test)]
mod tests;

mod cancel;
mod consent;
mod context;
mod delivery;
mod doctor;
mod envelope;
mod hold;
#[cfg(unix)]
mod private_to_user;
mod process_env;
mod process_identity;
mod provider;
mod provider_process;
mod query;
mod reopen;
use reopen::{
    read_resumed_from, refuse_follow_up_to_shared_conversation, reopen_refusal_gate, run_reopen,
};
mod self_test;
mod session;
mod session_start;

use session::turn::*;
use session::*;
mod settings;
mod terminal;
mod turn_report;
use turn_report::TurnReport;
#[cfg(test)]
use turn_report::finish_request_with_extra;
mod turn_run;

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
use terminal::ownership::MacTerminalAppIdentity;
#[cfg(test)]
use terminal::ownership::verify_terminal_owner_attestation;
#[cfg(windows)]
use terminal::ownership::verify_terminal_surface_ownership;
#[cfg(test)]
use terminal::ownership::{
    MacTerminalShellIdentity, NativeProcessIdentity, native_owner_identity_matches,
    verified_terminal_owner_process_group, verified_terminal_shell_process_group,
};
#[cfg(all(windows, test))]
use terminal::ownership::{current_native_session_owner, verified_windows_native_owner};
#[cfg(windows)]
use terminal::{windows_console_handle_path, windows_console_root_never_ran};

#[cfg(all(test, windows))]
use provider_process::command as provider_process_command;

#[cfg(any(windows, test))]
use std::fs::File;
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use agent_bridge::provider_effort_args;
use agent_bridge::{
    FirstPartyCli, checked_deadline_from, cli_version_is_supported, confirm_explicit_close,
    process_is_alive, terminal_safe_text, validate_terminal_input,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const DEFAULT_TIMEOUT_SECS: u64 = 900;

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
        request: ConsoleControlRequest,
        id: String,
    },
    Host(terminal::host::HostCommand),
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
    timeout: Duration,
}

#[derive(Debug)]
pub(crate) struct PruneRequest {
    closed_before_days: u64,
    explicit: bool,
    json: bool,
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

pub(crate) struct CommandSpec {
    pub(crate) name: &'static str,
    parse: fn(&str, &[String]) -> Result<NativeCommand>,
    // Usage remains handwritten; this flag is checked by its consistency test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) hidden: bool,
}

pub(crate) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "ask",
        parse: |_, args| parse_ask(args),
        hidden: false,
    },
    CommandSpec {
        name: "self-test",
        parse: |_, args| self_test::parse(args),
        hidden: false,
    },
    CommandSpec {
        name: "tell",
        parse: |_, args| parse_tell(args),
        hidden: false,
    },
    CommandSpec {
        name: "hold",
        parse: |_, args| hold::parse(args),
        hidden: false,
    },
    CommandSpec {
        name: "cancel",
        parse: |_, args| cancel::parse(args),
        hidden: false,
    },
    CommandSpec {
        name: "consent",
        parse: |_, args| parse_consent(args),
        hidden: false,
    },
    CommandSpec {
        name: "settings",
        parse: |_, args| parse_settings(args),
        hidden: false,
    },
    CommandSpec {
        name: "reopen",
        parse: |_, args| parse_reopen(args),
        hidden: false,
    },
    CommandSpec {
        name: "inspect",
        parse: |_, args| query::parse_inspect(args),
        hidden: false,
    },
    CommandSpec {
        name: "result",
        parse: |_, args| query::parse_result(args),
        hidden: false,
    },
    CommandSpec {
        name: "wait",
        parse: |_, args| query::wait::parse(args),
        hidden: false,
    },
    CommandSpec {
        name: "search",
        parse: |_, args| query::parse_search(args),
        hidden: false,
    },
    CommandSpec {
        name: "doctor",
        parse: |_, args| doctor::parse_args(args),
        hidden: false,
    },
    CommandSpec {
        name: "sessions",
        parse: |_, args| parse_sessions(args),
        hidden: false,
    },
    CommandSpec {
        name: "status",
        parse: |_, args| query::status::parse(args),
        hidden: false,
    },
    CommandSpec {
        name: "prune-sessions",
        parse: |_, args| parse_prune(args),
        hidden: false,
    },
    CommandSpec {
        name: "close-session",
        parse: |_, args| parse_close(args),
        hidden: false,
    },
    CommandSpec {
        name: "native-session",
        parse: |_, args| parse_session(args),
        hidden: true,
    },
    CommandSpec {
        name: "native-hook",
        parse: |_, args| parse_hook(args),
        hidden: true,
    },
    CommandSpec {
        name: "native-provider-control",
        parse: |_, args| parse_provider_control(args),
        hidden: true,
    },
    CommandSpec {
        name: "native-console-control",
        parse: |_, args| parse_console_control(args),
        hidden: true,
    },
    CommandSpec {
        name: terminal::host::CONSOLE_HOST_COMMAND,
        parse: |command, rest| terminal::host::parse(command, rest).map(NativeCommand::Host),
        hidden: true,
    },
    CommandSpec {
        name: terminal::host::ITERM2_HOST_COMMAND,
        parse: |command, rest| terminal::host::parse(command, rest).map(NativeCommand::Host),
        hidden: true,
    },
    CommandSpec {
        name: terminal::host::APPLE_TERMINAL_HOST_COMMAND,
        parse: |command, rest| terminal::host::parse(command, rest).map(NativeCommand::Host),
        hidden: true,
    },
    CommandSpec {
        name: terminal::host::WEZTERM_HOST_COMMAND,
        parse: |command, rest| terminal::host::parse(command, rest).map(NativeCommand::Host),
        hidden: true,
    },
    CommandSpec {
        name: terminal::host::GHOSTTY_HOST_COMMAND,
        parse: |command, rest| terminal::host::parse(command, rest).map(NativeCommand::Host),
        hidden: true,
    },
    CommandSpec {
        name: terminal::host::WARP_HOST_COMMAND,
        parse: |command, rest| terminal::host::parse(command, rest).map(NativeCommand::Host),
        hidden: true,
    },
];

pub(crate) fn is_command(value: &str) -> bool {
    COMMANDS.iter().any(|entry| entry.name == value)
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
    let entry = COMMANDS
        .iter()
        .find(|entry| entry.name == command)
        .with_context(|| format!("unknown native command: {command}"))?;
    (entry.parse)(command, rest)
}

fn parse_consent(rest: &[String]) -> Result<NativeCommand> {
    Ok(NativeCommand::Consent(rest.to_vec()))
}

fn parse_settings(rest: &[String]) -> Result<NativeCommand> {
    Ok(NativeCommand::Settings(rest.to_vec()))
}

fn parse_session(rest: &[String]) -> Result<NativeCommand> {
    let id = one_positional(rest, "native-session requires one session id")?;
    require_valid_session_id(id)?;
    Ok(NativeCommand::RunSession { id: id.to_owned() })
}

fn parse_provider_control(rest: &[String]) -> Result<NativeCommand> {
    let (provider, arguments) = rest
        .split_first()
        .context("native-provider-control requires a provider")?;
    let provider = FirstPartyCli::from_str(provider).map_err(anyhow::Error::msg)?;
    Ok(NativeCommand::ProviderControl {
        provider,
        arguments: arguments.to_vec(),
    })
}

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
    fn needs_only_bound_surface(&self) -> bool {
        matches!(self, Self::Close | Self::Window { .. })
    }
}

fn parse_console_control(rest: &[String]) -> Result<NativeCommand> {
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

#[derive(Default)]
struct CommonStartOptions {
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
    title: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    terminal: Option<terminal::TerminalKind>,
    yolo: bool,
    timeout: Option<Duration>,
    detach: bool,
    json: bool,
}

// Consume one common option, leaving unknown tokens untouched for the command.
fn parse_common_start_option(
    options: &[String],
    index: &mut usize,
    parsed: &mut CommonStartOptions,
) -> Result<bool> {
    match options[*index].as_str() {
        "--prompt" => set_once(
            &mut parsed.prompt,
            option_value(options, index, "--prompt")?.to_owned(),
            "--prompt",
        )?,
        "--prompt-file" => set_once(
            &mut parsed.prompt_file,
            PathBuf::from(option_value(options, index, "--prompt-file")?),
            "--prompt-file",
        )?,
        "--title" => set_once(
            &mut parsed.title,
            option_value(options, index, "--title")?.to_owned(),
            "--title",
        )?,
        "--model" => set_once(
            &mut parsed.model,
            option_value(options, index, "--model")?.to_owned(),
            "--model",
        )?,
        "--effort" => set_once(
            &mut parsed.effort,
            option_value(options, index, "--effort")?.to_owned(),
            "--effort",
        )?,
        "--terminal" => set_once(
            &mut parsed.terminal,
            terminal::TerminalKind::from_str(option_value(options, index, "--terminal")?)
                .map_err(anyhow::Error::msg)?,
            "--terminal",
        )?,
        "--timeout-secs" => {
            let value = option_value(options, index, "--timeout-secs")?;
            set_once(&mut parsed.timeout, parse_timeout(value)?, "--timeout-secs")?;
        }
        "--yolo" => set_flag_once(&mut parsed.yolo, "--yolo")?,
        "--detach" => set_flag_once(&mut parsed.detach, "--detach")?,
        "--json" => set_flag_once(&mut parsed.json, "--json")?,
        _ => return Ok(false),
    }
    *index += 1;
    Ok(true)
}

fn parse_ask(args: &[String]) -> Result<NativeCommand> {
    let (provider, options) = args
        .split_first()
        .context("ask requires codex, claude, agy, or pi")?;
    let provider = FirstPartyCli::from_str(provider).map_err(anyhow::Error::msg)?;
    let mut workspace = None;
    let mut parsed = CommonStartOptions::default();
    let mut context_results = Vec::new();
    let mut index = 0;
    while index < options.len() {
        if parse_common_start_option(options, &mut index, &mut parsed)? {
            continue;
        }
        match options[index].as_str() {
            "--workspace" => set_once(
                &mut workspace,
                PathBuf::from(option_value(options, &mut index, "--workspace")?),
                "--workspace",
            )?,
            "--context-result" => context::push_option(
                &mut context_results,
                option_value(options, &mut index, "--context-result")?,
            )?,
            option => bail!("unknown ask option: {option}"),
        }
        index += 1;
    }
    let CommonStartOptions {
        prompt,
        prompt_file,
        title,
        model,
        effort,
        terminal,
        yolo,
        timeout,
        detach,
        json,
    } = parsed;
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
    let mut parsed = CommonStartOptions::default();
    let mut index = 0;
    while index < options.len() {
        if parse_common_start_option(options, &mut index, &mut parsed)? {
            continue;
        }
        bail!("unknown reopen option: {}", options[index]);
    }
    let CommonStartOptions {
        prompt,
        prompt_file,
        title,
        model,
        effort,
        terminal,
        yolo,
        timeout,
        detach,
        json,
    } = parsed;
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
    let mut timeout = None;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--explicit" => set_flag_once(&mut explicit, "--explicit")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            "--timeout-secs" => {
                index += 1;
                let value = options
                    .get(index)
                    .context("--timeout-secs requires a value")?;
                let duration = parse_timeout(value)?;
                if duration > Duration::from_secs(3600) {
                    bail!("close-session --timeout-secs must be in 1..=3600");
                }
                set_once(&mut timeout, duration, "--timeout-secs")?;
            }
            option => bail!("unknown close-session option: {option}"),
        }
        index += 1;
    }
    confirm_explicit_close(explicit)?;
    Ok(NativeCommand::Close(CloseRequest {
        id: id.to_owned(),
        explicit,
        json,
        timeout: timeout.unwrap_or(CLOSE_TIMEOUT),
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
        NativeCommand::RunSession { id } => launch::wrapper::run(&id),
        NativeCommand::Hook { provider, payload } => run_hook(provider, payload.as_deref()),
        NativeCommand::ProviderControl {
            provider,
            arguments,
        } => provider::run_control(provider, &arguments),
        NativeCommand::ConsoleControl { request, id } => run_windows_console_control(request, &id),
        NativeCommand::Host(command) => terminal::host::run(command),
    }
}

fn valid_pending_prompt_name(value: &str) -> bool {
    value.starts_with("pending-prompt-")
        && value.ends_with(".txt")
        && value.len() <= 128
        && Path::new(value).file_name().and_then(|name| name.to_str()) == Some(value)
}

#[cfg(target_os = "windows")]
fn run_windows_console_control(request: ConsoleControlRequest, id: &str) -> Result<()> {
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
fn run_windows_console_control(_request: ConsoleControlRequest, _id: &str) -> Result<()> {
    bail!("native Windows console control is only available on Windows")
}

fn run_ask(request: AskRequest) -> Result<()> {
    let mut report = TurnReport::new(request.json, serde_json::Map::new());
    let outcome = run_ask_inner(request, &mut report);
    report.finish(outcome, serde_json::Map::new())
}

fn run_ask_inner(request: AskRequest, report: &mut TurnReport) -> Result<()> {
    let options = session_start::StartOptions {
        timeout: request.timeout,
        detach: request.detach,
        json: request.json,
    };
    session_start::start(options, &mut session_start::FreshSource(request), report)
}

// Closes a managed session's own visible surface exactly as an explicit `close-session`
// does, through the same terminal-close authority checks. `reason` is kept in the closed
// status when the close itself reports nothing, so a session closed because of a detected
// conflict still says why.
fn close_session_surface(
    directory: &Path,
    id: &str,
    reason: Option<String>,
    deadline: std::time::Instant,
) -> Result<()> {
    session::close::close(
        &Store::open_unchecked(directory),
        reason,
        deadline,
        |session| terminal::ownership::close_owned_surface(directory, id, session, deadline),
    )
    .map(|_| ())
}

fn run_tell(request: TellRequest) -> Result<()> {
    let mut report = TurnReport::new(
        request.json,
        serde_json::Map::from_iter([("session".into(), serde_json::json!(request.id))]),
    );
    let outcome = run_tell_inner(request, &mut report);
    let mut extra = serde_json::Map::new();
    if let Some(gate) = outcome.as_ref().err().and_then(reopen_refusal_gate) {
        extra.insert("gate".to_owned(), serde_json::json!(gate));
    }
    report.finish(outcome, extra)
}

fn run_tell_inner(request: TellRequest, report: &mut TurnReport) -> Result<()> {
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    // Attached results are resolved and pinned before the target is recovered, repaired,
    // claimed, or sent to, so a failed resolution leaves every session unchanged.
    let attached = context::resolve(&request.context_results)?;
    let directory = Reader::session_directory(&request.id)?;
    session::hold::permit(&Reader::open_unchecked(&directory), &request.id)?;
    Store::open_unchecked(&directory).converge_until(deadline)?;
    let manifest = Reader::open_unchecked(&directory).manifest()?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    let terminal_session: terminal::TerminalSession =
        Reader::open_unchecked(&directory).terminal()?;
    delivery::verify_terminal_surface_ownership_until(
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
    let resumed_from = read_resumed_from(&directory)?;
    let (mut claim, baseline) = turn::claim_ready(
        &Store::open_unchecked(&directory),
        &request.id,
        &attached.sources,
    )?;
    report.addressed(request.id.clone(), claim.receipt().request_id.clone());
    refuse_follow_up_to_shared_conversation(
        provider,
        &directory,
        &request.id,
        resumed_from.as_ref(),
        deadline,
        &mut claim,
        &previous_state,
    )?;
    turn_run::run(
        report,
        claim,
        delivery::Turn::FollowUp { prompt: &prompt },
        &turn_run::TurnRun {
            facts: turn_run::SessionFacts {
                id: &request.id,
                directory: &directory,
                provider,
                provider_path: &manifest.provider_path,
                surface: &terminal_session,
            },
            timing: turn_run::Timing {
                deadline,
                requested: request.timeout,
            },
            output: turn_run::Output {
                detach: request.detach,
                json: request.json,
            },
            extra: &serde_json::Map::new(),
            baseline,
        },
        |error| error,
    )
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
        for entry in
            session::enumerate::enumerate(root, session::enumerate::CheckOrder::FileTypeFirst)?
        {
            let entry = entry.map_err(session::enumerate::Fault::into_io)?;
            let directory = entry.directory;
            let Ok(manifest) = Reader::open_unchecked(&directory).manifest() else {
                continue;
            };
            if !(session::enumerate::Scope {
                workspace: request.workspace.as_ref(),
                provider: request.provider,
            })
            .admits(&manifest)
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
            // Observation only, whether or not this listing repaired above.
            let mut origin_budget = session::origin::ReadBudget::for_query(None);
            let origin = session::origin::observe(
                root,
                session::origin::read_reference(
                    &Reader::open_unchecked(&directory),
                    &mut origin_budget,
                ),
                &mut origin_budget,
            );
            let mut value = serde_json::json!({
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
                "results": Reader::open_unchecked(&directory).published_result_count().unwrap_or(0),
            });
            origin.add_fields(&mut value);
            sessions.push(value);
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
    for entry in session::enumerate::enumerate(root, session::enumerate::CheckOrder::FileTypeFirst)?
    {
        let entry = entry.map_err(session::enumerate::Fault::into_io)?;
        let id = entry.id;
        let directory = entry.directory;
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
        let closed = match Reader::open_unchecked(&directory).closed_if_present() {
            Ok(Some(closed)) => closed,
            Ok(None) | Err(_) => continue,
        };
        let status = match Reader::open_unchecked(&directory).status_if_present() {
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
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    confirm_explicit_close(request.explicit)?;
    let directory = Reader::session_directory(&request.id)?;
    close_session_surface(&directory, &request.id, None, deadline)
        .with_context(|| format!("failed to close visible terminal session {}", request.id))?;
    // Reported after the close succeeded, within what is left of its deadline. The scan
    // reads records only and never fails the close.
    let started_from = session::origin::scan_started_from(
        directory.parent().unwrap_or(Path::new("")),
        &request.id,
        deadline.min(Instant::now() + session::origin::SCAN_TIME),
    );
    if request.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "session": request.id,
                "closed": true,
                "started_from": started_from,
            }))?
        );
    } else {
        println!("closed {}", request.id);
        for line in started_from_lines(&request.id, &started_from) {
            println!("{}", terminal_safe_text(&line, false));
        }
    }
    Ok(())
}

fn started_from_lines(id: &str, started_from: &session::origin::StartedFrom) -> Vec<String> {
    let mut lines = Vec::new();
    if !started_from.open.is_empty() {
        lines.push(format!(
            "{} session(s) started from {id} remain unclosed in the observed records: {}",
            started_from.open.len(),
            started_from.open.join(" ")
        ));
        lines.push(format!(
            "Close a session you no longer need with '{PUBLIC_COMMAND} close-session ID --explicit'."
        ));
    }
    if started_from.incomplete {
        lines.push("Started-from scan incomplete; the list may omit sessions.".to_owned());
    }
    lines
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
    create_session_within(&Reader::state_root()?, &Reader::home_directories(), spec)
}

/// [`create_session`] with the directories whose own entries are taken as durable
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
