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

// Claude's launch-dialog policy: Claude answers its exact startup workspace dialog.
pub(super) struct ClaudeLaunchDialog;

impl consent::LaunchDialogAnswerer for ClaudeLaunchDialog {
    fn policy<'a>(
        &'a self,
        launch: consent::LaunchFacts<'a>,
    ) -> Box<dyn consent::LaunchDialogPolicy + 'a> {
        Box::new(ClaudeDialogPolicy { launch })
    }
}

struct ClaudeDialogPolicy<'a> {
    launch: consent::LaunchFacts<'a>,
}

impl consent::LaunchDialogPolicy for ClaudeDialogPolicy<'_> {
    fn dialog_key(&self, screen: &str) -> Option<terminal::DialogKey> {
        dialog_key(
            screen,
            self.launch.workspace,
            self.launch.bypass == consent::Bypass::On,
        )
    }

    // Claude Code 2.1.302 still shows workspace trust under bypass on native Windows (#28),
    // and so does 2.1.321 on macOS (2026-11-10); 2.1.319 on macOS started without it. A
    // version does not say which, so a macOS launch under bypass answers a dialog that is
    // there and stops waiting once the completion module reports the session's start.
    fn bypass_start(&self) -> Option<&dyn consent::BypassStart> {
        if cfg!(windows) { None } else { Some(self) }
    }

    // 2.1.302 guards input for 150 ms and remounts its selector. One second allows margin
    // for an idle renderer; equal pixels do not prove settlement. Replace with an official
    // addressed workspace-approval capability.
    fn settle(&self) -> Duration {
        if cfg!(windows) {
            Duration::from_secs(1)
        } else {
            Duration::ZERO
        }
    }
}

impl consent::BypassStart for ClaudeDialogPolicy<'_> {
    fn started(&self) -> Result<consent::ProviderStart> {
        Ok(
            match super::completion::started_under_bypass(self.launch.directory)? {
                None => consent::ProviderStart::NoSignal,
                Some(false) => consent::ProviderStart::NotStarted,
                Some(true) => consent::ProviderStart::Started,
            },
        )
    }

    fn not_started(&self) -> String {
        super::completion::readiness_timeout(self.launch.directory)
    }
}

// Claude exposes no documented process-local trust switch. Use only its exact
// startup workspace dialog; no config write, permission dialog, or generic Enter
// fallback. Delete when Claude ships an official trust launch option.
//
// Claude Code 2.1.322 adds a paragraph to the same dialog when the folder's
// `.claude/settings.local.json` pre-approves tool permissions (observed 2026-11-10, macOS).
// "Yes" there also accepts those pre-approvals, so the variant is answered only for a
// session launched under bypass, where Claude asks for no tool permission anyway (owner's
// decision, 2026-11-10). Only the observed form is recognized: two permissions, the plural
// "tool permissions" and that file name.
pub(super) fn dialog_key(
    screen: &str,
    workspace: &Path,
    bypass: bool,
) -> Option<terminal::DialogKey> {
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
    let access = lines
        .iter()
        .position(|s| *s == "Claude Code'll be able to read, edit, and execute files here.")?;
    if access < 3 || access >= guide || guide + 4 != lines.len() {
        return None;
    }
    if access + 1 != guide && !(bypass && pre_approval_notice(&lines[access + 1..guide])) {
        return None;
    }
    let safety = lines[2..access].join(" ");
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

// The paragraph between the access sentence and the guide link, joined across wrapped rows:
// the count, then exactly that many permissions joined by " and ", then the closing
// sentence. A permission is `Tool` or `Tool(rule)` without whitespace, so further prose, a
// second notice or a permission wrapped across rows does not match. Only two permissions
// were observed; the separators of a longer list are unknown and not guessed.
fn pre_approval_notice(lines: &[&str]) -> bool {
    let notice = lines.join(" ");
    let Some(rest) = notice.strip_prefix("⚠ This folder pre-approves ") else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let Ok(count) = rest[..digits].parse::<usize>() else {
        return false;
    };
    let Some(listed) = rest[digits..]
        .strip_prefix(" tool permissions in .claude/settings.local.json: ")
        .and_then(|rest| {
            rest.strip_suffix(
                " These will apply without asking. Only proceed if you trust this configuration.",
            )
        })
    else {
        return false;
    };
    let permissions: Vec<_> = listed.split(" and ").collect();
    count == 2 && permissions.len() == count && permissions.iter().all(|p| permission(p))
}

fn permission(text: &str) -> bool {
    let name = text.split('(').next().unwrap_or_default();
    let rule = &text[name.len()..];
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !text.chars().any(char::is_whitespace)
        && (rule.is_empty() || (rule.len() > 2 && rule.ends_with(')')))
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
