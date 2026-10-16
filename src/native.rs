use crate::native::session::primitives::{require_valid_session_id, unix_ms};
use crate::native::session::schema::{SESSION_SCHEMA, SessionManifest};
use crate::native::session::{CoreRecord, Reader, SessionState, Store, launch, update_status};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::str::FromStr;
#[cfg(test)]
mod tests;

mod args;
mod cancel;
mod clock;
mod close;
mod consent;
mod console_control;
mod context;
mod delivery;
mod doctor;
mod envelope;
mod hold;
mod hook;
#[cfg(unix)]
mod private_to_user;
mod process_env;
mod process_identity;
mod provider;
mod provider_process;
mod prune;
mod query;
mod reopen;
use reopen::run_reopen;
mod self_test;
mod session;
mod session_start;
pub(crate) use session_start::AskRequest;
mod sessions;
pub(crate) use sessions::SessionsRequest;

mod settings;
mod tell;
mod terminal;
mod turn_report;
mod turn_run;

const DEFAULT_TIMEOUT_SECS: u64 = 900;

#[derive(Debug)]
pub(crate) enum NativeCommand {
    Consent(Vec<String>),
    Settings(Vec<String>),
    Ask(AskRequest),
    SelfTest(self_test::Request),
    Tell(tell::TellRequest),
    Hold(hold::Request),
    Cancel(cancel::Request),
    Reopen(reopen::ReopenRequest),
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
    Prune(prune::PruneRequest),
    Close(close::CloseRequest),
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
        request: console_control::ConsoleControlRequest,
        id: String,
    },
    Host(terminal::host::HostCommand),
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
        parse: |_, args| session_start::parse_ask(args),
        hidden: false,
    },
    CommandSpec {
        name: "self-test",
        parse: |_, args| self_test::parse(args),
        hidden: false,
    },
    CommandSpec {
        name: "tell",
        parse: |_, args| tell::parse(args),
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
        parse: |_, args| reopen::parse_reopen(args),
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
        parse: |_, args| sessions::parse(args),
        hidden: false,
    },
    CommandSpec {
        name: "status",
        parse: |_, args| query::status::parse(args),
        hidden: false,
    },
    CommandSpec {
        name: "prune-sessions",
        parse: |_, args| prune::parse(args),
        hidden: false,
    },
    CommandSpec {
        name: "close-session",
        parse: |_, args| close::parse(args),
        hidden: false,
    },
    CommandSpec {
        name: "native-session",
        parse: |_, args| parse_session(args),
        hidden: true,
    },
    CommandSpec {
        name: "native-hook",
        parse: |_, args| hook::parse(args),
        hidden: true,
    },
    CommandSpec {
        name: "native-provider-control",
        parse: |_, args| parse_provider_control(args),
        hidden: true,
    },
    CommandSpec {
        name: "native-console-control",
        parse: |_, args| console_control::parse(args),
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
    let id = args::one_positional(rest, "native-session requires one session id")?;
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

pub(crate) fn run(command: NativeCommand) -> Result<()> {
    match command {
        NativeCommand::Consent(args) => consent::run(&args),
        NativeCommand::Settings(args) => settings::run(&args),
        NativeCommand::Ask(request) => session_start::run_ask(request),
        NativeCommand::SelfTest(request) => self_test::run(request),
        NativeCommand::Tell(request) => tell::run(request),
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
        NativeCommand::Sessions(request) => sessions::run(request),
        NativeCommand::Status(request) => query::status::run(request),
        NativeCommand::Prune(request) => prune::run(request),
        NativeCommand::Close(request) => close::run(request),
        NativeCommand::RunSession { id } => launch::wrapper::run(&id),
        NativeCommand::Hook { provider, payload } => hook::run(provider, payload.as_deref()),
        NativeCommand::ProviderControl {
            provider,
            arguments,
        } => provider::run_control(provider, &arguments),
        NativeCommand::ConsoleControl { request, id } => console_control::run(request, &id),
        NativeCommand::Host(command) => terminal::host::run(command),
    }
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
