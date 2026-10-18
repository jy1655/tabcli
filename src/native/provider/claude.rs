use super::{
    InitialPromptDisposition, LaunchContext, LaunchPlan, NativeProviderAdapter, NoWatcher,
    ReopenSupport, ResumeContext, ResumePlan, ResumedSessionContext,
};
#[cfg(test)]
use crate::native::delivery::Readiness;
use crate::native::delivery::{self, Transport};
#[cfg(test)]
use crate::native::session::SessionState;
use crate::native::session::turn;
use crate::native::session::{CoreRecord, Reader, RecordReader, RecordStore, Store, record};
use agent_bridge::{
    FirstPartyCli, provider_effort_args, provider_launch_args, provider_model_args,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use super::super::terminal;

mod messenger;
mod registry;

use messenger::{ClaudeMessage, run_message_guard, run_message_receipt};

#[cfg(test)]
pub(in crate::native) use registry::override_session_registry_for_test;
use registry::{
    claude_sessions_registry_dir, live_conversation_writer, live_process_creation_time,
    recheck_resumed_conversation_holders, wait_for_resumed_conversation_holders,
};

pub(super) static ADAPTER: ClaudeAdapter = ClaudeAdapter;

static CROSS_SESSION_TURN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(super) struct ClaudeAdapter;

const PENDING_TURN_FILE: &str = "claude-pending-turn.json";
// Drop caller-session markers from managed and messenger launches, preserving user configuration; nested children have no inbox.
// Remove when Claude stops deriving identity from inherited markers; see docs/verification/2026-10-11-macos-0.4.1.md.
// The two CODEX_COMPANION_ names are set by the Claude Code Codex plugin's SessionStart hook
// (openai-codex/codex 1.0.6, scripts/session-lifecycle-hook.mjs) to the caller's session id and
// transcript path, and its tracked-jobs runtime reads the id to attribute jobs to the current
// session (#149). The plugin's app-server endpoint, pid-file and log-file names are user
// configuration and stay.
const CLAUDE_CODE_SESSION_MARKERS: &[&str] = &[
    "AI_AGENT",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_PID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_EFFORT",
    "CODEX_COMPANION_SESSION_ID",
    "CODEX_COMPANION_TRANSCRIPT_PATH",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PendingCrossSessionTurn {
    schema: u32,
    request_id: String,
    marker: String,
}

impl PendingCrossSessionTurn {
    fn new(request_id: &str) -> Result<Self> {
        if !(request_id.starts_with("claude-turn-")
            && request_id.len() <= 128
            && request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
        {
            bail!("invalid Claude cross-session turn id")
        }
        Ok(Self {
            schema: 1,
            request_id: request_id.to_owned(),
            marker: format!("<!-- agent-bridge-claude-turn:{request_id} -->"),
        })
    }
}

struct PendingTurnFile {
    path: PathBuf,
    retained: bool,
}

impl PendingTurnFile {
    fn retain(mut self) {
        self.retained = true;
    }
}

impl Drop for PendingTurnFile {
    fn drop(&mut self) {
        if !self.retained {
            let _ = RecordStore::at(&self.path).remove();
        }
    }
}

fn new_cross_session_turn_id() -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    Ok(format!(
        "claude-turn-{now}-{}-{}",
        std::process::id(),
        CROSS_SESSION_TURN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

// Claude's home has two defaults. Without `CLAUDE_CONFIG_DIR` the trust store is
// `<home>/.claude.json` and the session registry `<home>/.claude/sessions`; a non-empty
// override holds both, `<override>/.claude.json` and `<override>/sessions`, and is kept
// as spelled, relative or not. An empty override is unset for both (the registry always
// read it so; the trust store read it as the relative `.claude.json` through 0.4.12).
fn config_dir_override(env: &super::super::consent::Environment) -> Option<PathBuf> {
    env.claude_config_dir
        .as_deref()
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

// The trust store requires the user home even when the override names the store.
fn trust_store(env: &super::super::consent::Environment) -> Result<PathBuf> {
    let home = super::super::consent::user_home(env)?;
    Ok(config_dir_override(env)
        .unwrap_or_else(|| PathBuf::from(home))
        .join(".claude.json"))
}

impl NativeProviderAdapter for ClaudeAdapter {
    fn result_timeout_diagnostic(&self) -> Option<super::super::doctor::ResultTimeoutDiagnostic> {
        None
    }

    fn launch_trust(&self) -> crate::native::consent::LaunchTrust {
        crate::native::consent::LaunchTrust {
            answers_launch_dialog: true,
            // Claude Code 2.1.302 still shows workspace trust under bypass on native Windows (#28).
            bypass_suppresses_dialog: !cfg!(windows),
            applied_marker: None,
        }
    }

    fn initial_transport(&self) -> Box<dyn Transport> {
        initial_transport_for_platform(cfg!(windows))
    }
    fn follow_up_transport(&self) -> Box<dyn Transport> {
        Box::new(ClaudeMessage)
    }

    fn cancel_support(&self, _directory: &Path) -> Result<super::CancelSupport> {
        Ok(super::CancelSupport::Unsupported(
            "in this release the Bridge integration for claude does not support cancel".to_owned(),
        ))
    }

    fn workspace_trust_key(&self, screen: &str, workspace: &Path) -> Option<terminal::DialogKey> {
        claude_trust_prompt_key(screen, workspace)
    }
    fn workspace_trust(
        &self,
        workspace: &Path,
        env: &super::super::consent::Environment,
        store: Option<&Path>,
    ) -> Result<super::super::consent::Trust> {
        use super::super::consent::{self, Evidence, Trust};
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

    fn probe_environment_removals(&self) -> &'static [&'static str] {
        // A `claude --version` or doctor probe is not an interactive session, but it is
        // still a `claude` started by the bridge: it drops the same markers so that no
        // bridge-run Claude process is classified by an inherited session identity.
        CLAUDE_CODE_SESSION_MARKERS
    }

    fn diagnose(
        &self,
        context: super::super::doctor::Context<'_>,
    ) -> Vec<super::super::doctor::Check> {
        use super::super::doctor::{Availability::*, Check};
        let setting = context.directory.map(|directory| {
            RecordReader::at(&directory.join("claude-settings.json"))
                .optional_json::<serde_json::Value>()
        });
        let (availability, reason, detail) = match setting {
            Some(Ok(Some(value))) if value.get("crossSessionInbound").and_then(serde_json::Value::as_str) == Some("accept") =>
                (Available, "claude_inbound_configured", "The session settings record crossSessionInbound=accept. This does not prove the running CLI loaded it.".to_owned()),
            Some(Ok(Some(_))) => (Unknown, "claude_inbound_unverified", "The session file does not establish inbound acceptance; effective provider policy is unknown.".to_owned()),
            Some(Err(error)) => (Unknown, "claude_settings_unreadable", format!("{error:#}")),
            _ => (Unknown, "claude_settings_unavailable", "No managed session settings were observed.".to_owned()),
        };
        let inherited_markers = CLAUDE_CODE_SESSION_MARKERS
            .iter()
            .copied()
            .filter(|marker| std::env::var_os(marker).is_some())
            .collect::<Vec<_>>();
        let (markers_availability, markers_reason, markers_detail) = if inherited_markers.is_empty()
        {
            (
                Available,
                "claude_caller_markers_absent",
                "The doctor process inherited no Claude Code session markers.".to_owned(),
            )
        } else {
            (
                Available,
                "claude_caller_markers_removed_at_launch",
                format!(
                    "The caller is running inside a Claude Code session ({}). Managed launches and messengers drop these markers so the managed session registers as an independent top-level session; an unmanaged nested claude inherits them and stays undiscoverable.",
                    inherited_markers.join(", ")
                ),
            )
        };
        let mut checks = vec![
            Check::new(
                "claude_inbound_setting",
                availability,
                reason,
                detail,
                "Inspect the session settings and provider policy; doctor does not change either.",
            ),
            Check::new(
                "claude_caller_markers",
                markers_availability,
                markers_reason,
                markers_detail,
                "No action; this observes the doctor's own environment, not the running managed session.",
            )
            .evidence(serde_json::json!({ "inherited": inherited_markers })),
            Check::new(
                "claude_messaging",
                Unknown,
                "claude_runtime_gates_unverified",
                "Follow-up uses official ListAgents/SendMessage. Backend, feature flags, policy, and live discovery have not been verified; --probe only reads the CLI version.",
                "Use the official provider's availability diagnostics. Doctor never calls a messenger/model or substitutes terminal injection.",
            ),
        ];
        if let Some(check) = context
            .directory
            .and_then(resumed_conversation_holders_check)
        {
            checks.push(check);
        }
        checks
    }

    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        prepare_launch_for_platform(context, cfg!(windows))
    }

    fn reopen(&self) -> Result<&'static dyn ReopenSupport> {
        Ok(&CLAUDE_REOPEN)
    }

    fn handle_hook(&self, directory: &Path, payload: &serde_json::Value) -> Result<()> {
        handle_hook(directory, payload)
    }

    fn run_control(&self, arguments: &[String]) -> Result<()> {
        match arguments {
            [action, request @ ..] if action == "message-guard" => run_message_guard(request),
            [action, request_id] if action == "message-receipt" => run_message_receipt(request_id),
            _ => bail!("unsupported Claude Agent Bridge provider control"),
        }
    }
}

struct ClaudeReopen;
static CLAUDE_REOPEN: ClaudeReopen = ClaudeReopen;

impl ReopenSupport for ClaudeReopen {
    fn verify_available(&self, provider_session_id: &str) -> Result<()> {
        verify_reopen_available_for_platform(provider_session_id, cfg!(windows))?;
        let registry =
            claude_sessions_registry_dir(&super::super::consent::Environment::current())?;
        if let Some(pid) =
            live_conversation_writer(&registry, provider_session_id, live_process_creation_time)?
        {
            bail!(
                "reopen unsupported: Claude conversation {provider_session_id} is held by live Claude Code process {pid} registered under {}",
                registry.display()
            )
        }
        Ok(())
    }

    fn prepare(&self, context: ResumeContext<'_>) -> Result<ResumePlan> {
        prepare_resume_for_platform(context, cfg!(windows))
    }

    fn other_holders_after_registration(
        &self,
        context: ResumedSessionContext<'_>,
        deadline: Instant,
    ) -> Result<Vec<u32>> {
        verify_reopen_available_for_platform(context.provider_session_id, cfg!(windows))?;
        let registry =
            claude_sessions_registry_dir(&super::super::consent::Environment::current())?;
        let managed_name = managed_session_name(context.directory)?;
        wait_for_resumed_conversation_holders(
            &registry,
            context.provider_session_id,
            &managed_name,
            live_process_creation_time,
            deadline,
        )
    }

    fn other_holders_now(&self, context: ResumedSessionContext<'_>) -> Result<Vec<u32>> {
        verify_reopen_available_for_platform(context.provider_session_id, cfg!(windows))?;
        let registry =
            claude_sessions_registry_dir(&super::super::consent::Environment::current())?;
        let managed_name = managed_session_name(context.directory)?;
        recheck_resumed_conversation_holders(
            &registry,
            context.provider_session_id,
            &managed_name,
            live_process_creation_time,
        )
    }
}

// Claude exposes no documented process-local trust switch. Use only its exact
// startup workspace dialog; no config write, permission dialog, or generic Enter
// fallback. Delete when Claude ships an official trust launch option.
fn claude_trust_prompt_key(screen: &str, workspace: &Path) -> Option<terminal::DialogKey> {
    require_exact_claude_trust_scope(workspace).ok()?;
    let lines: Vec<_> = screen
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let start = lines.iter().rposition(|s| *s == "Accessing workspace:")?;
    let lines = &lines[start..];
    let native = super::super::consent::native_key(workspace).ok()?;
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

fn git_workspace_command(workspace: &Path) -> Result<std::process::Command> {
    let mut command = crate::native::process_env::helper_command("git");
    command
        .arg("-C")
        .arg(super::super::consent::native_key(workspace)?)
        .args(["rev-parse", "--show-toplevel"]);
    Ok(command)
}

fn require_exact_claude_trust_scope(workspace: &Path) -> Result<()> {
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
    let mut command = git_workspace_command(workspace)?;
    let output = crate::native::process_env::command_output_until(
        &mut command,
        Instant::now() + Duration::from_secs(2),
        "Claude workspace trust scope",
    )?;
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

fn initial_transport_for_platform(windows: bool) -> Box<dyn Transport> {
    if windows {
        Box::new(ClaudeMessage)
    } else {
        Box::new(delivery::LAUNCH_ARGUMENT)
    }
}

// The requested policy, in Claude's flags, ahead of every adapter argument: the bypass
// only when requested, then effort, then model.
fn policy_arguments(
    yolo: bool,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Vec<OsString>> {
    let mut arguments = provider_launch_args(FirstPartyCli::Claude, yolo)
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    if let Some(effort) = effort {
        arguments.extend(
            provider_effort_args(FirstPartyCli::Claude, effort)?
                .into_iter()
                .map(OsString::from),
        );
    }
    if let Some(model) = model {
        arguments.extend(
            provider_model_args(FirstPartyCli::Claude, model)
                .into_iter()
                .map(OsString::from),
        );
    }
    Ok(arguments)
}

// The complete launch command. Off Windows the initial transport is the launch argument,
// so the prompt is the last positional argument and is accepted at spawn; on Windows the
// cross-session message delivers it after launch and argv carries no prompt.
fn prepare_launch_for_platform(context: LaunchContext<'_>, windows: bool) -> Result<LaunchPlan> {
    let settings_path = context.directory.join("claude-settings.json");
    RecordStore::at(&settings_path).write_json(&hook_settings(context.bridge_executable))?;
    let mut arguments = policy_arguments(context.yolo, context.model, context.effort)?;
    arguments.extend([
        OsString::from("--settings"),
        settings_path.into_os_string(),
        OsString::from("--name"),
        OsString::from(managed_session_name(context.directory)?),
    ]);
    let initial_prompt =
        InitialPromptDisposition::of(initial_transport_for_platform(windows).as_ref());
    if initial_prompt == InitialPromptDisposition::AcceptedAtSpawn {
        arguments.push(OsString::from(context.prompt));
    }
    Ok(LaunchPlan {
        arguments,
        initial_prompt,
        watcher: Box::new(NoWatcher),
        environment_removals: CLAUDE_CODE_SESSION_MARKERS,
    })
}

// Forward only explicitly requested policy flags; Claude owns restored policy and defaults.
// Holder scans detect conflicts only at boundaries; see docs/verification/2026-10-12-reopen-source-notes.md.
// The plan carries no prompt: the reopened session receives it through the initial
// transport, exactly as a fresh launch does.
fn prepare_resume_for_platform(context: ResumeContext<'_>, windows: bool) -> Result<ResumePlan> {
    verify_reopen_available_for_platform(context.provider_session_id, windows)?;
    let settings_path = context.directory.join("claude-settings.json");
    RecordStore::at(&settings_path).write_json(&hook_settings(context.bridge_executable))?;
    let mut arguments = policy_arguments(context.yolo, context.model, context.effort)?;
    arguments.extend([
        OsString::from("--resume"),
        OsString::from(context.provider_session_id),
        OsString::from("--settings"),
        settings_path.into_os_string(),
        OsString::from("--name"),
        OsString::from(managed_session_name(context.directory)?),
    ]);
    Ok(ResumePlan {
        arguments,
        initial_prompt: InitialPromptDisposition::of(
            initial_transport_for_platform(windows).as_ref(),
        ),
        watcher: Box::new(NoWatcher),
        environment_removals: CLAUDE_CODE_SESSION_MARKERS,
    })
}

// Removable per platform once the macOS ownership check against the session registry has
// been verified live; the Windows check reuses the process creation-time identity that the
// console transport already verifies.
fn verify_reopen_available_for_platform(provider_session_id: &str, windows: bool) -> Result<()> {
    if !valid_claude_conversation_id(provider_session_id) {
        bail!(
            "reopen unsupported: recorded Claude conversation id {provider_session_id:?} is not a Claude session UUID"
        )
    }
    if !windows {
        bail!(
            "reopen unsupported: Claude reopen is implemented only for native Windows in this slice; the macOS check of ~/.claude/sessions ownership has not been verified live"
        )
    }
    Ok(())
}

fn valid_claude_conversation_id(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(index, character)| match index {
            8 | 13 | 18 | 23 => character == '-',
            _ => character.is_ascii_hexdigit(),
        })
}

// Doctor scans other live holders now, with the delivery gate's best-effort check.
// It proves nothing between scans; non-resumed sessions return None.
fn resumed_conversation_holders_check(directory: &Path) -> Option<super::super::doctor::Check> {
    use super::super::doctor::{Availability::*, Check};
    // The manifest's optional `resumed_from` projection, read as the manifest is read.
    let manifest = Reader::open_unchecked(directory)
        .record(CoreRecord::Manifest)
        .bytes()
        .ok()
        .flatten()
        .and_then(|bytes| record::decode::<serde_json::Value>(&bytes).ok())?;
    let provider_session_id = manifest
        .get("resumed_from")?
        .get("provider_session_id")?
        .as_str()?
        .to_owned();
    let managed_name = managed_session_name(directory).ok()?;
    let scan = claude_sessions_registry_dir(&super::super::consent::Environment::current())
        .and_then(|registry| {
            recheck_resumed_conversation_holders(
                &registry,
                &provider_session_id,
                &managed_name,
                live_process_creation_time,
            )
        });
    let next_action = "Detection is best-effort and taken only at launch, before each delivery, and now; a foreign claude --resume can still interleave between checks. Close the other holder before the next tell, or accept the shared transcript.";
    Some(match scan {
        Ok(others) if others.is_empty() => Check::new(
            "claude_resumed_conversation_holders",
            Available,
            "claude_resumed_conversation_exclusive_now",
            format!(
                "No other live Claude Code process is registered for resumed conversation {provider_session_id} at this moment."
            ),
            next_action,
        )
        .evidence(serde_json::json!({ "provider_session_id": provider_session_id, "other_holders": [] })),
        Ok(others) => Check::new(
            "claude_resumed_conversation_holders",
            Unavailable,
            "claude_resumed_conversation_shared",
            format!(
                "Resumed conversation {provider_session_id} is also held by live Claude Code process(es) {}; the next tell to this session will be refused with gate reopen-conflict while they stay registered.",
                others
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            next_action,
        )
        .evidence(serde_json::json!({ "provider_session_id": provider_session_id, "other_holders": others })),
        Err(error) => Check::new(
            "claude_resumed_conversation_holders",
            Unknown,
            "claude_resumed_conversation_unverified",
            format!(
                "The registry scan for resumed conversation {provider_session_id} failed: {error:#}; the next tell to this session will be refused with gate reopen-verification-failed until it succeeds."
            ),
            next_action,
        )
        .evidence(serde_json::json!({ "provider_session_id": provider_session_id })),
    })
}

fn correlated_response<'a>(message: &'a str, pending: &PendingCrossSessionTurn) -> Result<&'a str> {
    let trimmed = message.trim_end();
    let body = trimmed
        .strip_suffix(&pending.marker)
        .context("Claude response did not end with the expected turn marker")?
        .trim_end();
    if body.is_empty() {
        bail!("Claude correlated response contained no assistant text")
    }
    Ok(body)
}

fn managed_session_name(directory: &Path) -> Result<String> {
    let recipient = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("Claude managed session id is not UTF-8")?
        .to_owned();
    super::super::require_valid_session_id(&recipient)?;
    Ok(recipient)
}

fn claude_string<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

fn claude_owned_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    claude_string(payload, key).map(str::to_owned)
}

fn handle_hook(directory: &Path, payload: &serde_json::Value) -> Result<()> {
    match payload
        .get("hook_event_name")
        .and_then(serde_json::Value::as_str)
    {
        Some("StopFailure") => handle_stop_failure(directory, payload),
        Some("Stop") => handle_correlated_stop(directory, payload),
        Some(event) => bail!("unsupported Claude hook event: {event}"),
        None => bail!("Claude hook payload has no hook_event_name"),
    }
}

fn handle_correlated_stop(directory: &Path, payload: &serde_json::Value) -> Result<()> {
    let Some(pending_text) = Reader::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .text()?
    else {
        return handle_uncorrelated_stop(directory, payload);
    };
    let pending: PendingCrossSessionTurn =
        serde_json::from_str(&pending_text).context("failed to parse the pending Claude turn")?;
    let valid_pending = matches!(
        PendingCrossSessionTurn::new(&pending.request_id),
        Ok(expected) if expected.schema == pending.schema && expected.marker == pending.marker
    );
    if !valid_pending {
        bail!("Agent Bridge rejected invalid Claude turn correlation state")
    }
    let Some(message) = claude_string(payload, "last_assistant_message") else {
        return Ok(());
    };
    let Ok(message) = correlated_response(message, &pending) else {
        return Ok(());
    };
    turn::Report::for_claim(
        &Store::open_unchecked(directory),
        agent_bridge::FirstPartyCli::Claude,
        None,
    )
    .complete(
        message,
        claude_owned_string(payload, "session_id"),
        Some(pending.request_id),
    )
    .context("failed to record the correlated Claude result")
}

fn handle_uncorrelated_stop(directory: &Path, payload: &serde_json::Value) -> Result<()> {
    let message = claude_string(payload, "last_assistant_message")
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .context("Claude Stop hook payload has no assistant result")?;
    turn::Report::initial(
        &Store::open_unchecked(directory),
        agent_bridge::FirstPartyCli::Claude,
    )
    .complete(message, claude_owned_string(payload, "session_id"), None)
}

fn handle_stop_failure(directory: &Path, payload: &serde_json::Value) -> Result<()> {
    if Reader::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .text()?
        .is_some()
    {
        // SendMessage does not return the target prompt identity that would let
        // Agent Bridge bind this failure to the delivered request. Leave the
        // pending turn claimed instead of attributing an unrelated failure.
        return Ok(());
    }
    let error = payload
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown Claude API error");
    let detail = payload
        .get("error_details")
        .and_then(serde_json::Value::as_str)
        .filter(|detail| !detail.trim().is_empty());
    let error = detail.map_or_else(
        || format!("Claude turn failed: {error}"),
        |detail| format!("Claude turn failed: {error}: {detail}"),
    );
    turn::Report::initial(
        &Store::open_unchecked(directory),
        agent_bridge::FirstPartyCli::Claude,
    )
    .fail(&error, claude_owned_string(payload, "session_id"), None)?;
    Ok(())
}

fn install_pending_turn(directory: &Path, request_id: &str) -> Result<PendingTurnFile> {
    let pending = PendingCrossSessionTurn::new(request_id)?;
    let path = Reader::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .path()
        .to_owned();
    Store::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .write_json(&pending)?;
    Ok(PendingTurnFile {
        path,
        retained: false,
    })
}

pub(super) fn hook_settings(executable: &Path) -> serde_json::Value {
    let mut settings = serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": executable,
                    "args": ["native-hook", "claude"],
                    "timeout": 10
                }]
            }],
            "StopFailure": [{
                "hooks": [{
                    "type": "command",
                    "command": executable,
                    "args": ["native-hook", "claude"],
                    "timeout": 10
                }]
            }]
        }
    });
    settings
        .as_object_mut()
        .expect("Claude settings are an object")
        .insert(
            "crossSessionInbound".to_owned(),
            serde_json::Value::String("accept".to_owned()),
        );
    settings
}

#[cfg(test)]
mod tests;
