//! Argument helpers shared by the native command parsers.
use super::terminal;
use agent_bridge::checked_deadline_from;
use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

pub(super) fn one_positional<'a>(args: &'a [String], message: &str) -> Result<&'a str> {
    match args {
        [value] => Ok(value),
        _ => bail!("{message}"),
    }
}

pub(super) fn option_value<'a>(
    args: &'a [String],
    index: &mut usize,
    option: &str,
) -> Result<&'a str> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .with_context(|| format!("{option} requires a value"))
}

pub(super) fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        bail!("{option} may only be specified once");
    }
    Ok(())
}

pub(super) fn set_flag_once(flag: &mut bool, option: &str) -> Result<()> {
    if *flag {
        bail!("{option} may only be specified once");
    }
    *flag = true;
    Ok(())
}

pub(super) fn parse_timeout(value: &str) -> Result<Duration> {
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

pub(super) fn read_prompt_option(
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

#[derive(Default)]
pub(super) struct CommonStartOptions {
    pub(super) prompt: Option<String>,
    pub(super) prompt_file: Option<PathBuf>,
    pub(super) title: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) terminal: Option<terminal::TerminalKind>,
    pub(super) yolo: bool,
    pub(super) timeout: Option<Duration>,
    pub(super) detach: bool,
    pub(super) json: bool,
}

// Consume one common option, leaving unknown tokens untouched for the command.
pub(super) fn parse_common_start_option(
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

// The canonical form of an explicit `--workspace`, the one `sessions`, `search` and `status`
// compare against the manifest: an unresolved absolute path is kept, a relative one is
// joined to the current directory, and an I/O error is the caller's.
pub(super) fn resolve_explicit_workspace(path: PathBuf) -> std::io::Result<PathBuf> {
    path.canonicalize().or_else(|_| {
        if path.is_absolute() {
            Ok(path)
        } else {
            std::env::current_dir().map(|cwd| cwd.join(path))
        }
    })
}

// The scope of a command that reads the current workspace when neither option names one
// (`search`, `status`). `sessions` has no `--all-workspaces` and lists every workspace by
// default, so it resolves its explicit path alone.
pub(super) enum WorkspaceScope {
    Workspace(PathBuf),
    AllWorkspaces,
}

pub(super) fn workspace_scope(
    command: &str,
    workspace: Option<PathBuf>,
    all_workspaces: bool,
) -> Result<WorkspaceScope> {
    match (workspace, all_workspaces) {
        (Some(_), true) => bail!("{command} accepts only one of --workspace or --all-workspaces"),
        (None, true) => Ok(WorkspaceScope::AllWorkspaces),
        (Some(path), false) => Ok(WorkspaceScope::Workspace(resolve_explicit_workspace(path)?)),
        (None, false) => Ok(WorkspaceScope::Workspace(
            std::env::current_dir()
                .and_then(|cwd| cwd.canonicalize())
                .context(
                    "failed to resolve the current workspace; pass --workspace or --all-workspaces",
                )?,
        )),
    }
}
