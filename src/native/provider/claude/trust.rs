use crate::native::{
    consent::{self, Evidence, Trust},
    terminal,
};
use anyhow::{Context, Result, bail};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

// Claude's home has two defaults. Without `CLAUDE_CONFIG_DIR` the trust store is
// `<home>/.claude.json` and the session registry `<home>/.claude/sessions`; a non-empty
// override holds both, `<override>/.claude.json` and `<override>/sessions`, and is kept
// as spelled, relative or not. An empty override is unset for both (the registry always
// read it so; the trust store read it as the relative `.claude.json` through 0.4.12).
pub(super) fn config_dir_override(env: &consent::Environment) -> Option<PathBuf> {
    env.claude_config_dir
        .as_deref()
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

// The trust store requires the user home even when the override names the store.
pub(super) fn trust_store(env: &consent::Environment) -> Result<PathBuf> {
    let home = consent::user_home(env)?;
    Ok(config_dir_override(env)
        .unwrap_or_else(|| PathBuf::from(home))
        .join(".claude.json"))
}

pub(super) fn workspace_trust(
    workspace: &Path,
    env: &consent::Environment,
    store: Option<&Path>,
) -> Result<Trust> {
    require_exact_claude_trust_scope(workspace)?;
    let store = match store {
        Some(store) => store.to_path_buf(),
        None => trust_store(env)?,
    };
    let Some(text) = consent::read_store(&store)? else {
        return Ok(Trust::Absent);
    };
    let config: serde_json::Value = serde_json::from_str(&text)?;
    let config = config
        .as_object()
        .context("Claude config is not an object")?;
    let Some(projects) = config.get("projects") else {
        return Ok(Trust::Absent);
    };
    let projects = projects
        .as_object()
        .context("Claude projects is not an object")?;
    let native = consent::native_key(workspace)?;
    let key = if cfg!(windows) {
        native.replace('\\', "/")
    } else {
        native.clone()
    };
    let entry = projects.get(&key).or_else(|| projects.get(&native));
    let entry = entry
        .map(|v| {
            v.as_object()
                .context("Claude project trust entry is not an object")
        })
        .transpose()?;
    match entry.and_then(|v| v.get("hasTrustDialogAccepted")) {
        Some(serde_json::Value::Bool(true)) => Ok(Trust::Trusted(Evidence {
            provider: "claude".into(),
            store,
            key,
        })),
        Some(serde_json::Value::Bool(false)) | None => Ok(Trust::Absent),
        _ => bail!("unknown Claude workspace trust value"),
    }
}

// Claude exposes no documented process-local trust switch. Use only its exact
// startup workspace dialog; no config write, permission dialog, or generic Enter
// fallback. Delete when Claude ships an official trust launch option.
pub(super) fn dialog_key(screen: &str, workspace: &Path) -> Option<terminal::DialogKey> {
    require_exact_claude_trust_scope(workspace).ok()?;
    let lines: Vec<_> = screen
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let start = lines.iter().rposition(|s| *s == "Accessing workspace:")?;
    let lines = &lines[start..];
    let native = consent::native_key(workspace).ok()?;
    let key = if cfg!(windows) {
        native.replace('\\', "/")
    } else {
        native.clone()
    };
    if lines.len() < 8
        || (lines[1] != native && lines[1] != key)
        || *lines.last()? != "Enter to confirm · Esc to cancel"
    {
        return None;
    }
    let guide = lines.iter().position(|s| *s == "Security guide")?;
    if guide < 4
        || guide + 4 != lines.len()
        || lines[guide - 1] != "Claude Code'll be able to read, edit, and execute files here."
    {
        return None;
    }
    let safety = lines[2..guide - 1].join(" ");
    if safety
        != "Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to review what's in this folder first."
    {
        return None;
    }
    match (lines[guide + 1], lines[guide + 2]) {
        ("❯ No, exit", "Yes, I trust this folder") => Some(terminal::DialogKey::DownEnter),
        ("No, exit", "❯ Yes, I trust this folder") => Some(terminal::DialogKey::Enter),
        _ => None,
    }
}

pub(super) fn git_workspace_command(workspace: &Path) -> Result<std::process::Command> {
    let mut command = crate::native::process_env::helper_command("git");
    command
        .arg("-C")
        .arg(consent::native_key(workspace)?)
        .args(["rev-parse", "--show-toplevel"]);
    Ok(command)
}

pub(super) fn require_exact_claude_trust_scope(workspace: &Path) -> Result<()> {
    require_exact_scope_with(workspace, &mut ProcessGitScope)
}

trait GitScope {
    fn output(&mut self, workspace: &Path) -> Result<std::process::Output>;
}

struct ProcessGitScope;
impl GitScope for ProcessGitScope {
    fn output(&mut self, workspace: &Path) -> Result<std::process::Output> {
        let mut command = git_workspace_command(workspace)?;
        crate::native::process_env::command_output_until(
            &mut command,
            Instant::now() + Duration::from_secs(2),
            "Claude workspace trust scope",
        )
    }
}

fn require_exact_scope_with(workspace: &Path, git: &mut impl GitScope) -> Result<()> {
    // Claude saves Git-subdirectory approval at the root; exact child consent cannot authorize it.
    // Let Git resolve worktrees/nested roots; failed lookup leaves Claude's dialog untouched.
    let mut has_git_ancestor = false;
    for parent in workspace.ancestors().skip(1) {
        if parent.join(".git").try_exists()? {
            has_git_ancestor = true;
            break;
        }
    }
    if !has_git_ancestor {
        return Ok(());
    }
    let output = git.output(workspace)?;
    if !output.status.success() {
        bail!("cannot verify Claude's Git workspace trust scope; use its own trust dialog");
    }
    let root = PathBuf::from(std::str::from_utf8(&output.stdout)?.trim_end_matches(['\r', '\n']))
        .canonicalize()?;
    if root != workspace {
        bail!(
            "Claude would approve the parent Git repository, not this exact workspace; use its own trust dialog"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
