use super::{
    CompletionMonitor, CrossSessionMessageContext, CrossSessionMessageFailure,
    CrossSessionMessageResult, FollowUpTransport, InitialPromptTransport, LaunchContext,
    LaunchPlan, NativeProviderAdapter, ResumeContext, ResumePlan, ResumedSessionContext,
};
use crate::native::session::{Reader, RecordReader, Store};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use super::super::terminal;

pub(super) static ADAPTER: CodexAdapter = CodexAdapter;

pub(super) struct CodexAdapter;

const PENDING_TURN_FILE: &str = "codex-pending-turn.json";
const MAX_NATIVE_QUEUE_OUTPUT_BYTES: usize = 1024 * 1024;
// A managed terminal can display a different thread or the /agent picker. Codex
// has no integrated, atomic active-thread check plus addressed terminal input.
// Replace this refusal only with a provider-owned input path that binds delivery
// to the recorded thread; a title, old notify, or live process is not that proof.
const UNADDRESSED_FOLLOW_UP: &str = "Codex terminal follow-up is unavailable: the active thread cannot be verified; no terminal input was sent. Use the thread-addressed native queue with Codex 0.149+; inspect the queue error and `agent-bridge doctor <session> --probe` for the unavailable prerequisite";

fn codex_version_supports_native_queue(output: &str) -> Result<bool> {
    let installed = output
        .split_whitespace()
        .filter_map(|token| {
            let candidate = token
                .trim_matches(|character: char| !character.is_ascii_alphanumeric())
                .strip_prefix('v')
                .unwrap_or(
                    token.trim_matches(|character: char| !character.is_ascii_alphanumeric()),
                );
            Version::parse(candidate).ok()
        })
        .next()
        .ok_or_else(|| {
            anyhow::anyhow!("could not parse a semantic Codex version from {output:?}")
        })?;
    Ok(installed >= Version::new(0, 149, 0))
}

fn valid_codex_thread_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[derive(Debug, Deserialize, Serialize)]
struct PendingCodexTurn {
    schema: u32,
    claim_token: String,
    marker: String,
}

impl PendingCodexTurn {
    fn new(claim_token: &str) -> Result<Self> {
        validate_claim_token(claim_token)?;
        Ok(Self {
            schema: 1,
            claim_token: claim_token.to_owned(),
            marker: format!("<!-- agent-bridge-codex-turn:{claim_token} -->"),
        })
    }
}

// Codex `resume <thread>` is official, but reopen needs two file-level gates this slice
// does not implement: the exclusive thread writer lock under ~/.codex/thread-writer-locks
// and rows for the thread in the queue store, whose accepted inputs would execute inside
// the reopened TUI. The resume-while-held behavior also needs a live check first.
const CODEX_REOPEN_UNSUPPORTED: &str = "reopen unsupported: Codex reopen is not implemented in this slice; it requires the thread writer-lock and queued_items gates and a live check of resume-while-held behavior";

impl NativeProviderAdapter for CodexAdapter {
    fn workspace_trust_key(&self, _screen: &str, _workspace: &Path) -> Option<terminal::DialogKey> {
        None
    }
    fn workspace_trust(
        &self,
        workspace: &Path,
        homes: &super::super::consent::Homes,
    ) -> Result<super::super::consent::Trust> {
        use super::super::consent::{self, Evidence, Trust};
        let Some(text) = consent::read_store(&homes.codex)? else {
            return Ok(Trust::Absent);
        };
        let config: toml::Value = text.parse().context("invalid Codex trust config")?;
        let mut key = consent::native_key(workspace)?;
        if cfg!(windows) {
            key = key.to_lowercase();
        }
        let Some(projects) = config.get("projects") else {
            return Ok(Trust::Absent);
        };
        let projects = projects
            .as_table()
            .context("Codex projects is not a table")?;
        let entry = projects.get(&key).or_else(|| {
            if cfg!(windows) {
                projects
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == key)
                    .map(|(_, v)| v)
            } else {
                None
            }
        });
        let entry = entry
            .map(|v| {
                v.as_table()
                    .context("Codex project trust entry is not a table")
            })
            .transpose()?;
        match entry.and_then(|v| v.get("trust_level")) {
            None => Ok(Trust::Absent),
            Some(v) if v.as_str() == Some("trusted") => Ok(Trust::Trusted(Evidence {
                provider: "codex".into(),
                store: homes.codex.clone(),
                key,
            })),
            Some(v) if v.as_str() == Some("untrusted") => Ok(Trust::Declined),
            _ => bail!("unknown Codex trust level"),
        }
    }

    fn probe_environment_removals(&self) -> &'static [&'static str] {
        // Codex derives no session identity from the caller's environment.
        &[]
    }

    fn diagnose(
        &self,
        context: super::super::doctor::Context<'_>,
    ) -> Vec<super::super::doctor::Check> {
        diagnose_codex(context)
    }

    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let claim_token = super::super::current_turn_claim_token(context.directory)?
            .context("Codex launch has no native turn claim")?;
        let pending = install_pending_turn(context.directory, &claim_token)?;
        let mut arguments = codex_launch_arguments(
            context.bridge_executable,
            context.workspace,
            context.prompt,
            &pending,
            cfg!(windows),
        )?;
        if super::super::consent::authorized(context.directory, FirstPartyCli::Codex)? {
            // Official process-local TOML override; never edits config.toml or any
            // approval/sandbox setting. Insert before the positional prompt.
            arguments.splice(
                0..0,
                [
                    OsString::from("-c"),
                    OsString::from(workspace_trust_override(context.workspace)?),
                ],
            );
            super::super::consent::applied(context.directory, "codex-config-override")?;
        }
        Ok(LaunchPlan {
            arguments,
            prompt_is_positional: false,
            completion_monitor: CompletionMonitor::Hook,
            environment_removals: &[],
        })
    }

    fn verify_reopen_available(&self, _provider_session_id: &str) -> Result<()> {
        bail!("{CODEX_REOPEN_UNSUPPORTED}")
    }

    fn prepare_resume(&self, _context: ResumeContext<'_>) -> Result<ResumePlan> {
        bail!("{CODEX_REOPEN_UNSUPPORTED}")
    }

    fn other_resumed_conversation_holders(
        &self,
        _context: ResumedSessionContext<'_>,
    ) -> Result<Vec<u32>> {
        bail!("{CODEX_REOPEN_UNSUPPORTED}")
    }

    fn initial_prompt_transport(&self) -> InitialPromptTransport {
        if cfg!(windows) {
            InitialPromptTransport::TerminalPasteAfterLaunch
        } else {
            InitialPromptTransport::ProviderArgument
        }
    }

    fn initial_prompt_ready_delay(&self) -> Duration {
        // Codex can render its composer before cold-start MCP and extension
        // initialization settles. Returns delivered during that redraw window
        // can be ignored while leaving the pasted prompt in the composer.
        Duration::from_secs(12)
    }

    fn send_initial_prompt(
        &self,
        session: &terminal::TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult {
        if cfg!(windows) {
            let directory = session
                .managed_session_id
                .as_deref()
                .context("Codex terminal has no managed session binding")
                .and_then(Reader::session_directory)
                .map_err(terminal::TerminalSendFailure::not_sent)?;
            // A screen read costs a helper process and the wait polls ten times a second,
            // so the screen is read once per `COMPOSER_POLL`.
            let mut last_read: Option<Instant> = None;
            super::super::consent::wait_for_native_trust(
                &directory,
                FirstPartyCli::Codex,
                deadline,
                &mut || {
                    if last_read.is_some_and(|read| read.elapsed() < COMPOSER_POLL) {
                        return false;
                    }
                    last_read = Some(Instant::now());
                    terminal::read_screen(session, deadline)
                        .is_ok_and(|screen| composer_is_ready(&screen))
                },
            )
            .map_err(terminal::TerminalSendFailure::not_sent)?;
        }
        terminal::send_file(session, prompt_path, deadline)
    }

    fn terminal_initial_prompt(&self, directory: &Path, prompt: &str) -> Result<String> {
        let pending = read_pending_turn(directory)?
            .context("Codex initial turn correlation state is missing")?;
        Ok(correlated_prompt(prompt, &pending))
    }

    #[cfg(any(windows, test))]
    fn terminal_submit_count(&self) -> usize {
        2
    }

    fn follow_up_transport(&self) -> FollowUpTransport {
        // This shared branch uses claim-based queue correlation rather than the
        // messenger's generated turn identity. The adapter refuses unaddressed
        // terminal fallback until Codex can bind it to the managed thread.
        FollowUpTransport::ProviderCrossSessionMessageWithTerminalPasteFallback
    }

    fn new_cross_session_turn_id(&self) -> Result<String> {
        bail!("Codex does not support provider cross-session turns")
    }

    fn send_cross_session_message(
        &self,
        context: CrossSessionMessageContext<'_>,
    ) -> CrossSessionMessageResult {
        send_native_queue_message(context).map_err(|failure| {
            if failure.allows_terminal_fallback() {
                CrossSessionMessageFailure::not_sent(
                    failure.into_error().context(UNADDRESSED_FOLLOW_UP),
                )
            } else {
                failure
            }
        })
    }

    fn handle_hook(&self, directory: &Path, payload: &serde_json::Value) -> Result<()> {
        let raw_message = codex_string(payload, "last-assistant-message")
            .map(|message| message.trim())
            .filter(|message| !message.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Codex notify payload has no assistant result"))?;
        let Some(pending) = read_pending_turn(directory)? else {
            return Ok(());
        };
        let message = match correlated_response(raw_message, &pending) {
            Ok(message) => message,
            Err(_) if codex_input_correlates(payload, &pending) => raw_message,
            Err(_) => return Ok(()),
        };
        let thread_id = codex_owned_string(payload, "thread-id");
        let established_thread = established_codex_thread(directory)?;
        if established_thread
            .as_deref()
            .is_some_and(|established| thread_id.as_deref() != Some(established))
        {
            return Ok(());
        }
        super::super::record_provider_result_for_claim(
            directory,
            FirstPartyCli::Codex,
            message,
            thread_id,
            codex_owned_string(payload, "turn-id"),
            Some(&pending.claim_token),
        )
        .context("failed to record the correlated Codex result")
    }

    fn run_control(&self, _arguments: &[String]) -> Result<()> {
        bail!("Codex does not expose Agent Bridge provider controls")
    }

    fn send_terminal_follow_up(
        &self,
        _session: &terminal::TerminalSession,
        _prompt_path: &Path,
        _deadline: Instant,
    ) -> terminal::TerminalSendResult {
        Err(terminal::TerminalSendFailure::not_sent(anyhow::anyhow!(
            UNADDRESSED_FOLLOW_UP
        )))
    }

    fn prepare_terminal_follow_up(
        &self,
        _directory: &Path,
        _prompt: &str,
        _claim_token: &str,
    ) -> Result<String> {
        bail!(UNADDRESSED_FOLLOW_UP)
    }

    fn cancel_terminal_follow_up(&self, directory: &Path, claim_token: &str) -> Result<()> {
        cancel_pending_turn(directory, claim_token)
    }
}

// The empty composer of the chat view: the prompt glyph and Codex's placeholder
// (`PLACEHOLDER` in codex-rs/tui/src/chatwidget.rs, rust-v0.159.3).
const COMPOSER_ROW: &str = "› Ask Codex to do anything";
const COMPOSER_POLL: Duration = Duration::from_secs(1);

// Positive evidence that an initial paste lands in the composer and not on the trust
// dialog. Codex draws the composer only when no onboarding screen and no view of the
// bottom pane is active, so its empty row on the screen means that the trust dialog is
// over. An exact project entry is not the only way there (issue #60): Codex takes the
// trust of a workspace without one from its repository root, which covers a
// subdirectory and a linked worktree, shows no dialog then and saves no exact entry;
// and "Trust and continue", answered by the user in the managed terminal, saves the
// repository root as well. Measured 2026-10-02 with Codex CLI 0.159.3 on native
// Windows: no dialog in a linked worktree of a trusted repository; the dialog in a
// directory that nothing trusts, with and without `--yolo`, and in a plain directory
// below a trusted one.
//
// Nothing is inferred from Codex's configuration or from Git: an inference that is
// wrong would paste onto the dialog. An empty screen, a screen that is still loading
// and a layout this does not know are no evidence, and neither is a composer beside
// the words of the dialog. This is not workspace consent, which stays exact and is the
// only trust that is shared with another provider. Delete it when Codex has an input
// path that needs no paste.
fn composer_is_ready(screen: &str) -> bool {
    let mut composer = false;
    for line in screen.lines() {
        if line.contains("Trust this folder?") || line.contains("Trust and continue") {
            return false;
        }
        composer |= line.trim() == COMPOSER_ROW;
    }
    composer
}

fn workspace_trust_override(workspace: &Path) -> Result<String> {
    let mut key = super::super::consent::native_key(workspace)?;
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    // Codex config/src/overrides.rs splits the left side on every dot and does
    // not parse quoted TOML keys. Put the path in the TOML VALUE instead: quoted
    // dotted keys on the left create a different project and leave the dialog up.
    Ok(format!(
        "projects={{{}={{trust_level=\"trusted\"}}}}",
        serde_json::to_string(&key)?
    ))
}

fn codex_launch_arguments(
    bridge_executable: &Path,
    workspace: &Path,
    prompt: &str,
    pending: &PendingCodexTurn,
    windows: bool,
) -> Result<Vec<OsString>> {
    let notify = serde_json::to_string(&[
        bridge_executable.to_string_lossy().as_ref(),
        "native-hook",
        "codex",
    ])?;
    let mut arguments = vec![
        OsString::from("-c"),
        OsString::from(format!("notify={notify}")),
    ];
    if !windows {
        arguments.extend([
            OsString::from("-C"),
            workspace.as_os_str().to_owned(),
            OsString::from(correlated_prompt(prompt, pending)),
        ]);
    }
    Ok(arguments)
}

fn codex_queue_arguments(
    thread_id: &str,
    prompt: &str,
    pending: &PendingCodexTurn,
) -> Vec<OsString> {
    vec![
        OsString::from("queue"),
        OsString::from("--thread"),
        OsString::from(thread_id),
        OsString::from("--message"),
        OsString::from(correlated_prompt(prompt, pending)),
    ]
}

#[derive(Debug)]
struct BoundedCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

#[derive(Debug)]
struct CodexCommandFailure {
    error: anyhow::Error,
    delivery_may_have_started: bool,
}

impl CodexCommandFailure {
    fn not_started(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_started: false,
        }
    }

    fn started(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_started: true,
        }
    }

    fn before_resume(error: anyhow::Error, created_suspended: bool) -> Self {
        Self {
            error,
            delivery_may_have_started: !created_suspended,
        }
    }
}

fn send_native_queue_message(context: CrossSessionMessageContext<'_>) -> CrossSessionMessageResult {
    let manifest = Reader::open_unchecked(context.directory)
        .manifest()
        .map_err(CrossSessionMessageFailure::not_sent)?;
    match codex_version_supports_native_queue(&manifest.provider_version) {
        Ok(true) => {}
        Ok(false) => {
            return Err(CrossSessionMessageFailure::terminal_fallback(
                anyhow::anyhow!(
                    "Codex {} predates native queue support in 0.149.0",
                    manifest.provider_version
                ),
            ));
        }
        Err(error) => return Err(CrossSessionMessageFailure::not_sent(error)),
    }
    let thread_id = established_codex_thread(context.directory)
        .map_err(CrossSessionMessageFailure::not_sent)?
        .ok_or_else(|| {
            CrossSessionMessageFailure::terminal_fallback(anyhow::anyhow!(
                "Codex session has no established provider thread id"
            ))
        })?;
    if !valid_codex_thread_id(&thread_id) {
        return Err(CrossSessionMessageFailure::terminal_fallback(
            anyhow::anyhow!("Codex session provider identity is not a thread UUID"),
        ));
    }
    // Codex owns backend selection: its queue command uses an available shared daemon or
    // an embedded server, and both write the provider's durable, thread-addressed queue.
    // See codex-rs/tui/src/session_queue_commands.rs and ext/queue/src/service.rs in
    // rust-v0.149.0 and rust-v0.160.0. Requiring a daemon here blocked the embedded path.
    // Queue acceptance is not completion; the target's correlated notify still proves it.

    let pending =
        PendingCodexTurn::new(context.request_id).map_err(CrossSessionMessageFailure::not_sent)?;
    let arguments = codex_queue_arguments(&thread_id, context.prompt, &pending);
    let mut command = super::super::provider_process::command(
        context.provider_path,
        context.directory,
        arguments,
    )
    .map_err(|error| {
        CrossSessionMessageFailure::terminal_fallback(
            error.context("failed to prepare the Codex queue command"),
        )
    })?;
    command
        .current_dir(&manifest.workspace)
        .env(super::super::SESSION_DIR_ENV, context.directory);
    write_pending_turn(context.directory, &pending)
        .map_err(CrossSessionMessageFailure::not_sent)?;
    let result = match run_bounded_command_until(&mut command, context.deadline, "Codex queue") {
        Ok(output) => classify_codex_queue_output(
            output.status.success(),
            &output.stdout,
            &output.stderr,
            output.truncated,
            &thread_id,
        ),
        Err(failure) if !failure.delivery_may_have_started => {
            Err(CrossSessionMessageFailure::terminal_fallback(failure.error))
        }
        Err(failure) => Err(CrossSessionMessageFailure::delivery_uncertain(
            failure.error,
        )),
    };
    if result
        .as_ref()
        .is_err_and(|failure| !failure.delivery_may_have_occurred())
    {
        let _ = cancel_pending_turn(context.directory, context.request_id);
    }
    result
}

fn diagnose_codex(context: super::super::doctor::Context<'_>) -> Vec<super::super::doctor::Check> {
    use super::super::doctor::{self, Availability::*, Check};
    let mut checks = Vec::new();
    let version = context
        .manifest
        .map(|m| m.provider_version.as_str())
        .or(context.current_version);
    let (availability, reason) = match version.map(codex_version_supports_native_queue) {
        Some(Ok(true)) => (Available, "codex_queue_version_supported"),
        Some(Ok(false)) => (Unavailable, "codex_queue_version_unsupported"),
        _ => (Unknown, "codex_queue_version_unknown"),
    };
    checks.push(Check::new("codex_queue_version", availability, reason,
        "Native queue requires Codex 0.149+. Existing sessions select transport using their launch-recorded version.",
        "Unavailable queue prerequisites refuse tell before terminal input: the active TUI thread cannot be verified. Do not resend an uncertain turn.")
        .evidence(serde_json::json!({"version": version, "source": if context.manifest.is_some() { "launch_record" } else { "current_probe" }})));
    if let Some(current) = context.current_version {
        let (availability, reason) = match codex_version_supports_native_queue(current) {
            Ok(true) => (Available, "codex_current_queue_version_supported"),
            Ok(false) => (Unavailable, "codex_current_queue_version_unsupported"),
            Err(_) => (Unknown, "codex_current_queue_version_unknown"),
        };
        checks.push(Check::new("codex_current_queue_version", availability, reason,
            "Currently installed queue client version; this does not replace the session's launch-recorded gate.",
            "Inspect both the launch version and current executable when an installation changes.")
            .evidence(serde_json::json!({"version": current})));
    }
    let thread = context.directory.map(established_codex_thread);
    let (availability, reason, evidence) = match thread {
        Some(Ok(Some(id))) if valid_codex_thread_id(&id) => (
            Available,
            "codex_thread_recorded",
            serde_json::json!({"thread_id": id}),
        ),
        Some(Ok(Some(_))) => (Unavailable, "codex_thread_invalid", serde_json::Value::Null),
        Some(Ok(None)) => (Unavailable, "codex_thread_missing", serde_json::Value::Null),
        Some(Err(error)) => (
            Unknown,
            "codex_thread_unreadable",
            serde_json::json!({"error": format!("{error:#}")}),
        ),
        None => (Unknown, "session_required", serde_json::Value::Null),
    };
    checks.push(Check::new("codex_thread", availability, reason,
        "Uses the same provider-owned event identity lookup as the queue sender. It does not prove a TUI currently has that thread loaded.",
        "Select a managed session with a recorded Codex result; a queue acceptance is not a completion.").evidence(evidence));
    let daemon = if !context.probe {
        Check::new(
            "codex_daemon",
            Unknown,
            "probe_not_requested",
            "The optional shared local daemon has not been queried. Its absence does not disable the native queue.",
            "Add --probe to observe codex app-server daemon version; doctor never starts the daemon. Codex queue selects its own backend.",
        )
    } else if !context.workspace.is_dir() {
        Check::new(
            "codex_daemon",
            Unknown,
            "workspace_unavailable",
            "The optional local daemon cannot be queried because the working directory is unavailable.",
            "Inspect the workspace check; doctor does not probe a daemon from a different workspace.",
        )
    } else if let Some(executable) = context.executable {
        match doctor::probe(
            executable,
            &["app-server", "daemon", "version"],
            Some(context.workspace),
            ADAPTER.probe_environment_removals(),
            context.deadline,
        ) {
            Ok(output) => diagnose_daemon_output(&output, version.unwrap_or("unknown")),
            Err(error) => Check::new(
                "codex_daemon",
                Unknown,
                "codex_daemon_probe_failed",
                format!("{error:#}"),
                "Inspect the local daemon separately; a failed observation does not prove its state.",
            ),
        }
    } else {
        Check::new(
            "codex_daemon",
            Unknown,
            "executable_unavailable",
            "The recorded provider executable cannot be probed.",
            "Check the provider executable before inspecting the daemon.",
        )
    };
    checks.push(daemon);
    checks.push(Check::new(
        "codex_terminal_follow_up",
        Unavailable,
        "codex_active_terminal_thread_unverified",
        "A managed terminal and recorded result do not identify the thread currently selected in the Codex TUI. Unaddressed terminal follow-up is refused before input.",
        "Use the thread-addressed native queue; inspect the queue version, thread and actual queue error. The daemon check is advisory; Agent Bridge does not start or restart the shared daemon.",
    ));
    checks.push(Check::new(
        "codex_mcp",
        Unknown,
        "codex_mcp_not_observed",
        "MCP connection and authentication state have not been observed. Version, daemon availability and completed model turns do not prove MCP startup succeeded.",
        "Inspect /mcp in this exact Codex session. A codex_apps startup failure with 401/token_revoked (or reauthenticationRequired) means the provider rejected the stored ChatGPT sign-in: every newly launched Codex reads the same stored sign-in and fails the same way until `codex login`, and a long-lived Codex process that still fails afterwards needs a restart to load the new one. `codex doctor` checks that sign-in outside a session (auth.credentials and the authenticated network.websocket_reachability handshake). Optional MCP failure does not by itself mean the model turn failed; doctor does not change authentication or retry prompts.",
    ));
    checks
}

fn diagnose_daemon_output(
    output: &std::process::Output,
    version: &str,
) -> super::super::doctor::Check {
    use super::super::doctor::{Availability::*, Check};
    // An optional server observation, not a delivery gate. A failed probe is not proof of absence.
    let classified = classify_codex_daemon_probe(
        output.status.success(),
        &output.stdout,
        &output.stderr,
        false,
        version,
    );
    let (availability, reason, detail) = match classified {
        Ok(()) => (Available, "codex_daemon_compatible", "A running compatible local daemon was observed. TUI liveness, queue acceptance, and completion were not tested.".to_owned()),
        Err(failure) if !output.status.success() => (Unavailable, "codex_daemon_unavailable", format!("The CLI rejected the optional shared daemon probe. This does not establish whether a daemon process exists or whether the native queue is available. {failure:#}")),
        Err(failure) => {
            let value = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
            let known = output.status.success() && value.as_ref().is_some_and(|v| {
                v.get("status").and_then(serde_json::Value::as_str).is_some_and(|status| status != "running")
                    || v.get("appServerVersion").and_then(serde_json::Value::as_str).is_some_and(|v| matches!(codex_version_supports_native_queue(v), Ok(false)))
            });
            (if known { Unavailable } else { Unknown }, if known { "codex_daemon_incompatible" } else { "codex_daemon_unverified" }, format!("{failure:#}"))
        }
    };
    Check::new(
        "codex_daemon",
        availability,
        reason,
        detail,
        "Codex queue selects a shared or embedded server; a shared daemon is not required. Its actual queue response determines delivery. Never resend a delivery-uncertain request.",
    ).evidence(serde_json::json!({"exit_code": output.status.code(), "stderr": String::from_utf8_lossy(&output.stderr).trim()}))
}

fn classify_codex_daemon_probe(
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
    output_truncated: bool,
    provider_version: &str,
) -> Result<()> {
    if output_truncated {
        bail!("Codex daemon probe output exceeded the safety limit");
    }
    if !success {
        bail!(
            "Codex local app-server daemon is unavailable for {provider_version}: {}",
            String::from_utf8_lossy(stderr).trim()
        );
    }
    let payload: serde_json::Value =
        serde_json::from_slice(stdout).context("Codex daemon probe returned invalid JSON")?;
    if payload.get("status").and_then(serde_json::Value::as_str) != Some("running") {
        bail!("Codex local app-server daemon did not report running status");
    }
    let app_server_version = payload
        .get("appServerVersion")
        .and_then(serde_json::Value::as_str)
        .context("Codex daemon probe did not report an app-server version")?;
    match codex_version_supports_native_queue(app_server_version) {
        Ok(true) => Ok(()),
        Ok(false) => bail!(
            "Codex local app-server {app_server_version} predates native queue support in 0.149.0"
        ),
        Err(error) => Err(error.context("Codex daemon reported an invalid app-server version")),
    }
}

fn run_bounded_command_until(
    command: &mut std::process::Command,
    deadline: Instant,
    label: &str,
) -> std::result::Result<BoundedCommandOutput, CodexCommandFailure> {
    if Instant::now() >= deadline {
        return Err(CodexCommandFailure::not_started(anyhow::anyhow!(
            "{label} timed out before it started"
        )));
    }
    let mut stdout = tempfile::tempfile()
        .with_context(|| format!("failed to create bounded stdout storage for {label}"))
        .map_err(CodexCommandFailure::not_started)?;
    let mut stderr = tempfile::tempfile()
        .with_context(|| format!("failed to create bounded stderr storage for {label}"))
        .map_err(CodexCommandFailure::not_started)?;
    let stdout_sink = stdout
        .try_clone()
        .with_context(|| format!("failed to clone bounded stdout storage for {label}"))
        .map_err(CodexCommandFailure::not_started)?;
    let stderr_sink = stderr
        .try_clone()
        .with_context(|| format!("failed to clone bounded stderr storage for {label}"))
        .map_err(CodexCommandFailure::not_started)?;
    super::super::provider_process::configure_process_tree(command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_sink))
        .stderr(Stdio::from(stderr_sink))
        .spawn()
        .with_context(|| format!("failed to start {label}"))
        .map_err(CodexCommandFailure::not_started)?;
    let process_tree = match super::super::provider_process::ProviderProcessTree::attach(&child) {
        Ok(process_tree) => process_tree,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            // Windows creates this child suspended, so an attach failure precedes any delivery.
            // Unix children are already running here and remain delivery-uncertain.
            return Err(CodexCommandFailure::before_resume(
                error.context(format!("failed to contain {label}")),
                cfg!(windows),
            ));
        }
    };
    if let Err(error) = process_tree.resume(&child) {
        terminate_bounded_process(&mut child, &process_tree);
        return Err(CodexCommandFailure::started(
            error.context(format!("failed to resume {label}")),
        ));
    }
    // The timeout fixture must observe its descendant before expiring the budget.
    // Release builds always keep the caller's original, shared deadline.
    #[cfg(all(test, unix))]
    let deadline = match tests::synchronized_queue_deadline(label, deadline) {
        Ok(deadline) => deadline,
        Err(error) => {
            terminate_bounded_process(&mut child, &process_tree);
            return Err(CodexCommandFailure::started(error));
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(20)),
                );
            }
            Ok(None) => {
                terminate_bounded_process(&mut child, &process_tree);
                return Err(CodexCommandFailure::started(anyhow::anyhow!(
                    "{label} timed out"
                )));
            }
            Err(error) => {
                terminate_bounded_process(&mut child, &process_tree);
                return Err(CodexCommandFailure::started(
                    anyhow::Error::new(error).context(format!("failed to wait for {label}")),
                ));
            }
        }
    };
    process_tree.terminate();
    let mut remaining_output_bytes = MAX_NATIVE_QUEUE_OUTPUT_BYTES;
    let (stdout, stdout_truncated) = read_capped_output(&mut stdout, &mut remaining_output_bytes)
        .with_context(|| format!("failed to read bounded stdout storage for {label}"))
        .map_err(CodexCommandFailure::started)?;
    let (stderr, stderr_truncated) = read_capped_output(&mut stderr, &mut remaining_output_bytes)
        .with_context(|| format!("failed to read bounded stderr storage for {label}"))
        .map_err(CodexCommandFailure::started)?;
    Ok(BoundedCommandOutput {
        status,
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
    })
}

fn terminate_bounded_process(
    child: &mut std::process::Child,
    process_tree: &super::super::provider_process::ProviderProcessTree,
) {
    process_tree.terminate();
    let _ = child.kill();
    let _ = child.wait();
}

fn read_capped_output(
    reader: &mut std::fs::File,
    remaining_bytes: &mut usize,
) -> Result<(Vec<u8>, bool)> {
    reader.seek(SeekFrom::Start(0))?;
    let mut output = Vec::new();
    reader
        .take(remaining_bytes.saturating_add(1) as u64)
        .read_to_end(&mut output)?;
    let truncated = output.len() > *remaining_bytes;
    output.truncate(*remaining_bytes);
    *remaining_bytes -= output.len();
    Ok((output, truncated))
}

fn classify_codex_queue_output(
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
    output_truncated: bool,
    thread_id: &str,
) -> CrossSessionMessageResult {
    if output_truncated {
        return Err(CrossSessionMessageFailure::delivery_uncertain(
            anyhow::anyhow!("Codex queue output exceeded the safety limit"),
        ));
    }

    let stdout = String::from_utf8_lossy(stdout);
    if success {
        let prefix = "Queued message ";
        let suffix = format!(" for thread {thread_id}.");
        let submission_id = stdout
            .trim()
            .strip_prefix(prefix)
            .and_then(|output| output.strip_suffix(&suffix));
        if submission_id.is_some_and(|id| !id.is_empty() && !id.chars().any(char::is_whitespace)) {
            return Ok(());
        }
        return Err(CrossSessionMessageFailure::delivery_uncertain(
            anyhow::anyhow!(
                "Codex queue reported success without the expected thread confirmation"
            ),
        ));
    }

    let stderr = String::from_utf8_lossy(stderr);
    let target_missing = stderr.contains(&format!("thread not found: {thread_id}"))
        || stderr.contains(&format!("no rollout found for thread id {thread_id}"));
    let native_queue_unavailable = stderr
        .contains("local app-server daemon does not support thread/queue/add")
        || stderr.contains("user message queue is unavailable")
        || stderr.contains(
            "cannot queue through an embedded app server while a local app-server daemon is running",
        );
    let rejected_before_enqueue = codex_queue_rejected_before_enqueue(&stderr, thread_id);
    let error = anyhow::anyhow!("Codex queue failed: {}", stderr.trim());
    if target_missing || native_queue_unavailable || rejected_before_enqueue {
        Err(CrossSessionMessageFailure::terminal_fallback(error))
    } else {
        Err(CrossSessionMessageFailure::delivery_uncertain(error))
    }
}

fn codex_queue_rejected_before_enqueue(stderr: &str, thread_id: &str) -> bool {
    if !stderr.contains("thread/queue/add failed:") {
        return false;
    }

    // Codex returns these before QueuedItemService::enqueue writes to the queue store.
    let invalid_request = stderr.contains("(code -32600)")
        && (stderr.contains("invalid thread id:")
            || stderr.contains(&format!(
                "ephemeral thread does not support queued submissions: {thread_id}"
            ))
            || stderr.contains(&format!("session {thread_id} is archived."))
            || stderr
                .contains("direct app-server input is not allowed for multi-agent v2 sub-agents")
            || stderr.contains(
                "direct app-server input is not allowed for unloaded spawned sub-agents",
            )
            || stderr.contains("queue cannot contain more than "));
    let input_too_large = stderr.contains("(code -32602)")
        && stderr.contains("Input exceeds the maximum length of 1048576 characters.");
    let thread_read_failed =
        stderr.contains("(code -32603)") && stderr.contains("failed to read thread:");
    invalid_request || input_too_large || thread_read_failed
}

fn validate_claim_token(claim_token: &str) -> Result<()> {
    if claim_token.is_empty()
        || claim_token.len() > 160
        || !claim_token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        bail!("invalid Codex turn claim token")
    }
    Ok(())
}

fn install_pending_turn(directory: &Path, claim_token: &str) -> Result<PendingCodexTurn> {
    let pending = PendingCodexTurn::new(claim_token)?;
    write_pending_turn(directory, &pending)?;
    Ok(pending)
}

fn write_pending_turn(directory: &Path, pending: &PendingCodexTurn) -> Result<()> {
    Store::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .write_json(pending)?;
    Ok(())
}

fn read_pending_turn(directory: &Path) -> Result<Option<PendingCodexTurn>> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .text()?
    else {
        return Ok(None);
    };
    let pending: PendingCodexTurn =
        serde_json::from_str(&text).context("failed to parse the pending Codex turn")?;
    let expected = PendingCodexTurn::new(&pending.claim_token)?;
    if pending.schema != expected.schema || pending.marker != expected.marker {
        bail!("Agent Bridge rejected invalid Codex turn correlation state")
    }
    Ok(Some(pending))
}

fn cancel_pending_turn(directory: &Path, claim_token: &str) -> Result<()> {
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(());
    };
    if pending.claim_token != claim_token {
        return Ok(());
    }
    Store::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .remove()
}

fn correlated_prompt(prompt: &str, pending: &PendingCodexTurn) -> String {
    format!(
        "{prompt}\n\n[Agent Bridge Codex turn metadata; do not include this metadata in the response]\n{}",
        pending.marker
    )
}

fn correlated_response<'a>(message: &'a str, pending: &PendingCodexTurn) -> Result<&'a str> {
    let body = message
        .trim_end()
        .strip_suffix(&pending.marker)
        .context("Codex response did not end with the expected turn marker")?
        .trim_end();
    if body.is_empty() {
        bail!("Codex correlated response contained no assistant text")
    }
    Ok(body)
}

fn codex_string<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

fn codex_owned_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    codex_string(payload, key).map(str::to_owned)
}

// The first line and the request heading of the IDE context that the TUI puts in front
// of the user's message while `/ide` is on (`render_prompt_context` and
// `PROMPT_REQUEST_BEGIN` in codex-rs/tui/src/ide_context/prompt.rs, rust-v0.159.3).
const IDE_CONTEXT_HEADING: &str = "# Context from my IDE setup:";
const IDE_REQUEST_HEADING: &str = "## My request for Codex:";

// Whether a message that Codex reports as the input of a turn is the prompt Bridge
// sent: it begins with the delegation header, directly or behind Codex's IDE context.
// The context can quote a file that holds the request heading, and so can the prompt,
// so every heading is tried.
fn begins_with_bridge_prompt(message: &str) -> bool {
    let header = super::super::NATIVE_DELEGATION_HEADER;
    let message = message.trim_start();
    message.starts_with(header)
        || (message.starts_with(IDE_CONTEXT_HEADING)
            && message
                .match_indices(IDE_REQUEST_HEADING)
                .any(|(at, heading)| {
                    message[at + heading.len()..]
                        .trim_start()
                        .starts_with(header)
                }))
}

// The input of the turn that Bridge started is the prompt Bridge sent: it begins with the
// delegation header and ends with the turn marker. The marker alone does not identify
// it. Codex CLI 0.159.3 answers the first message of a TUI with a second turn, on a
// thread of its own, that makes the task title; its input is Codex's instruction
// followed by the user's whole message, marker included, and its notify can arrive
// first (issue #61, 2026-10-02: `{"title":"READY"}` recorded as the result of two
// initial requests out of six, with the title thread as the session's thread). This
// tells Bridge's framing from that of a turn Codex starts; it is not an identity of
// the turn, which Codex does not give before the first notify.
fn codex_input_correlates(payload: &serde_json::Value, pending: &PendingCodexTurn) -> bool {
    if codex_string(payload, "type") != Some("agent-turn-complete") {
        return false;
    }
    payload
        .get("input-messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|messages| messages.iter().rev().find_map(serde_json::Value::as_str))
        .is_some_and(|message| {
            begins_with_bridge_prompt(message) && message.trim_end().ends_with(&pending.marker)
        })
}

fn established_codex_thread(directory: &Path) -> Result<Option<String>> {
    for path in Reader::open_unchecked(directory).events()? {
        let event: super::super::SessionEvent = RecordReader::at(&path).json()?;
        if event.provider == FirstPartyCli::Codex.as_str()
            && let Some(thread_id) = event.provider_session_id
        {
            return Ok(Some(thread_id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_trust_override_uses_a_toml_value_for_paths_with_dots_and_quotes() {
        for workspace in [
            Path::new("/work/space.dir/a\"b"),
            Path::new("C:\\Work\\한글.dir"),
        ] {
            let argument = workspace_trust_override(workspace).unwrap();
            let (key, value) = argument.split_once('=').unwrap();
            // The installed Codex splits this side with str::split('.'), without
            // TOML quoted-key parsing. It must therefore contain only 'projects'.
            assert_eq!(key, "projects");
            let parsed: toml::Value = format!("value={value}").parse().unwrap();
            let mut workspace_key = super::super::super::consent::native_key(workspace).unwrap();
            if cfg!(windows) {
                workspace_key = workspace_key.to_lowercase();
            }
            assert_eq!(
                parsed["value"][&workspace_key]["trust_level"].as_str(),
                Some("trusted")
            );
            assert_eq!(parsed["value"].as_table().unwrap().len(), 1);
        }
    }

    // The screens an initial paste can meet, as Codex CLI 0.159.3 drew them on
    // 2026-10-02 (session-kkpVsB: the dialog; session-nXZtvt: the composer), in the
    // order of a start in which the dialog opens and the user answers it.
    #[test]
    fn only_the_empty_composer_is_evidence_for_an_initial_paste() {
        let dialog = "  Folder access\n  C:\\work\\project\n  Trust this folder? Codex can read, edit, and run files here, subject to your permission settings. Folder settings\n  can run code automatically, even without a model request. Continue only if you trust these files. Your trust\n  decision will be saved.\n› 1. Trust and continue\n  2. Quit\n  enter continue · esc quit";
        let composer = "  >_ OpenAI Codex (v0.159.3)\n     D:\\Dev\\project\n  How deep does this codebase go?\n› Ask Codex to do anything                                   \n  GPT-6.1-Sol low · D:\\Dev\\project\n  ? for shortcuts";
        let start = [
            "",
            "\n\n\n",
            "  >_ OpenAI Codex (v0.159.3)\n",
            dialog,
            composer,
        ];
        assert_eq!(
            start.map(composer_is_ready),
            [false, false, false, false, true],
            "the gate opens with the composer and not before"
        );

        // Another row that begins with the glyph is not the composer: a selected
        // option, a draft, another placeholder.
        for screen in [
            "› 1. Trust and continue",
            "› Ask Codex to do anything else",
            "› Ask a follow-up question",
            "Ask Codex to do anything",
        ] {
            assert!(!composer_is_ready(screen), "{screen}");
        }
        // A screen that shows both is not one this knows.
        assert!(!composer_is_ready(&format!("{dialog}\n{composer}")));
        assert!(!composer_is_ready(&format!("{composer}\n{dialog}")));
    }

    #[test]
    fn workspace_trust_reads_exact_toml_key_and_no_permission_override() {
        use super::super::super::consent::{self, Trust};
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        let homes = consent::fixture_homes(tmp.path());
        let mut key = consent::native_key(&workspace).unwrap();
        if cfg!(windows) {
            key = key.to_lowercase();
        }
        let config = format!(
            "[projects.{}]\ntrust_level = \"trusted\"\n",
            serde_json::to_string(&key).unwrap()
        );
        // Created as Codex creates it in a user profile: nobody else can change it. A
        // plain file in the temporary directory inherits whatever that directory allows.
        super::super::super::write_private(&homes.codex, config.as_bytes()).unwrap();
        assert!(matches!(
            ADAPTER.workspace_trust(&workspace, &homes).unwrap(),
            Trust::Trusted(_)
        ));
        assert_eq!(
            ADAPTER
                .workspace_trust(&workspace.join("child"), &homes)
                .unwrap(),
            Trust::Absent
        );
        std::fs::write(
            &homes.codex,
            config.replace("trusted", "untrusted").as_bytes(),
        )
        .unwrap();
        assert_eq!(
            ADAPTER.workspace_trust(&workspace, &homes).unwrap(),
            Trust::Declined
        );
        std::fs::write(&homes.codex, b"[bad").unwrap();
        assert!(ADAPTER.workspace_trust(&workspace, &homes).is_err());
    }

    #[test]
    fn diagnostics_keep_launch_version_and_unprobed_daemon_observations_explicit() {
        use super::super::super::doctor::{Availability, Context};
        for (version, expected) in [
            (Some("0.148.0"), Availability::Unavailable),
            (Some("0.153.2"), Availability::Available),
            (Some("unknown"), Availability::Unknown),
            (None, Availability::Unknown),
        ] {
            let checks = super::diagnose_codex(Context {
                directory: None,
                manifest: None,
                executable: None,
                current_version: version,
                workspace: std::path::Path::new("."),
                probe: false,
                deadline: std::time::Instant::now(),
            });
            assert_eq!(
                checks
                    .iter()
                    .find(|c| c.id == "codex_queue_version")
                    .unwrap()
                    .availability,
                expected
            );
            assert_eq!(
                checks
                    .iter()
                    .find(|c| c.id == "codex_daemon")
                    .unwrap()
                    .availability,
                Availability::Unknown
            );
            assert_eq!(
                checks
                    .iter()
                    .find(|c| c.id == "codex_thread")
                    .unwrap()
                    .reason_code,
                "session_required"
            );
            let mcp = checks.iter().find(|c| c.id == "codex_mcp").unwrap();
            assert_eq!(mcp.availability, Availability::Unknown);
            assert_eq!(mcp.reason_code, "codex_mcp_not_observed");
            assert_eq!(
                checks
                    .iter()
                    .find(|c| c.id == "codex_terminal_follow_up")
                    .unwrap()
                    .availability,
                Availability::Unavailable
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn diagnostic_daemon_availability_describes_the_optional_shared_server() {
        use super::super::super::doctor::Availability::*;
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        for (success, payload, expected) in [
            (
                true,
                r#"{"status":"running","appServerVersion":"0.153.2"}"#,
                Available,
            ),
            (true, r#"{"status":"stopped"}"#, Unavailable),
            (
                true,
                r#"{"status":"running","appServerVersion":"0.148.0"}"#,
                Unavailable,
            ),
            (true, r#"{"status":"running"}"#, Unknown),
            (true, "invalid json", Unknown),
            (false, "", Unavailable),
        ] {
            let output = std::process::Output {
                status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
                stdout: payload.as_bytes().to_vec(),
                stderr: Vec::new(),
            };
            let check = super::diagnose_daemon_output(&output, "0.153.2");
            assert_eq!(check.availability, expected, "{payload}");
            let reported = serde_json::to_value(&check).unwrap();
            assert!(
                reported["next_action"]
                    .as_str()
                    .unwrap()
                    .contains("a shared daemon is not required")
            );
            assert_eq!(
                check.availability == Available,
                super::classify_codex_daemon_probe(
                    success,
                    &output.stdout,
                    &output.stderr,
                    false,
                    "0.153.2"
                )
                .is_ok()
            );
        }
    }

    use super::super::super::{SESSION_SCHEMA, SessionManifest, write_json_atomic};
    use super::super::super::{
        SessionEvent, SessionStatus, TURN_CLAIM_FILE, acquire_turn_claim, event_paths,
        native_delegation_prompt, read_json, update_status,
    };
    use super::*;

    #[cfg(unix)]
    fn write_successful_queue_provider(directory: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let provider = directory.join("fake-codex");
        std::fs::write(
            &provider,
            r#"#!/bin/sh
if [ "$1" = "app-server" ]; then
  printf '%s\0' "$@" > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/daemon-argv.bin"
  pwd > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/daemon-cwd.txt"
  printf '%s\n' '{"status":"running","appServerVersion":"0.153.2"}'
  exit 0
fi
if [ "$1" = "queue" ]; then
  printf '%s\0' "$@" > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/queue-argv.bin"
  pwd > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/queue-cwd.txt"
  printf 'Queued message queued-id for thread %s.\n' "$3"
  exit 0
fi
exit 91
"#,
        )
        .unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
        provider
    }

    fn write_queue_manifest(directory: &Path, provider: &Path, version: &str) {
        write_queue_manifest_for_workspace(directory, provider, version, directory);
    }

    fn write_queue_manifest_for_workspace(
        directory: &Path,
        provider: &Path,
        version: &str,
        workspace: &Path,
    ) {
        write_json_atomic(
            &directory.join("manifest.json"),
            &SessionManifest {
                schema: SESSION_SCHEMA,
                id: "session-codexqueue".to_owned(),
                provider: "codex".to_owned(),
                provider_path: provider.to_owned(),
                provider_version: version.to_owned(),
                workspace: workspace.to_owned(),
                title: "Codex queue test".to_owned(),
                model: None,
                effort: None,
                yolo: false,
                created_unix_ms: 1,
            },
        )
        .unwrap();
    }

    fn write_established_thread(directory: &Path, thread_id: &str) {
        std::fs::create_dir_all(directory.join("events")).unwrap();
        write_json_atomic(
            &directory.join("events/event-1.json"),
            &SessionEvent {
                provider: "codex".to_owned(),
                message: "initial result".to_owned(),
                error: None,
                provider_session_id: Some(thread_id.to_owned()),
                turn_id: Some("initial-turn".to_owned()),
                created_unix_ms: Some(1),
            },
        )
        .unwrap();
    }

    fn claim_pending_turn(directory: &Path) -> PendingCodexTurn {
        let claim = acquire_turn_claim(directory).unwrap();
        let token = claim.token.clone();
        claim.retain();
        install_pending_turn(directory, &token).unwrap()
    }

    fn marked(message: &str, pending: &PendingCodexTurn) -> String {
        format!("{message}\n{}", pending.marker)
    }

    #[test]
    fn codex_native_queue_requires_version_0_149_or_newer() {
        for version in ["codex-cli 0.147.0", "codex-cli 0.148.9"] {
            assert!(!codex_version_supports_native_queue(version).unwrap());
        }
        for version in ["codex-cli 0.149.0", "codex-cli 0.153.2"] {
            assert!(codex_version_supports_native_queue(version).unwrap());
        }
        assert!(codex_version_supports_native_queue("codex unknown").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_queue_never_selects_unaddressed_terminal_input() {
        use super::super::super::{
            CrossSessionFailureAction, acquire_ready_turn_claim_with_context,
            cross_session_failure_action,
        };
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session-codexqueue");
        std::fs::create_dir(&path).unwrap();
        let directory = path.as_path();
        let provider = directory.join("fake-codex");
        std::fs::write(
            &provider,
            r#"#!/bin/sh
if [ "$1" = "app-server" ]; then
  printf '%s\n' '{"status":"running","appServerVersion":"0.159.3"}'
  exit 0
fi
printf '%s\n' 'Error: failed to queue session message: thread/queue/add failed: user message queue is unavailable (code -32600)' >&2
exit 1
"#,
        )
        .unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
        write_queue_manifest(directory, &provider, "codex-cli 0.159.3");
        // The stored result belongs to A. It says nothing about whether the TUI
        // still shows A, has switched to B, or is displaying the agent picker.
        let managed_thread = "018f0000-0000-7000-8000-000000000001";
        write_established_thread(directory, managed_thread);
        update_status(directory, "ready", None, None).unwrap();
        let (claim, _) =
            acquire_ready_turn_claim_with_context(directory, "session-codexqueue", &[]).unwrap();
        update_status(directory, "working", None, None).unwrap();
        let failure = ADAPTER
            .send_cross_session_message(CrossSessionMessageContext {
                bridge_executable: Path::new("/unused/agent-bridge"),
                directory,
                provider_path: &provider,
                request_id: &claim.token,
                prompt: "request addressed only to A",
                deadline: Instant::now() + Duration::from_secs(2),
            })
            .unwrap_err();

        assert_eq!(
            cross_session_failure_action(ADAPTER.follow_up_transport(), &failure),
            CrossSessionFailureAction::ReturnError,
            "a queue rejection must not select a paste into an unverified active thread"
        );
        assert!(!failure.delivery_may_have_occurred());
        assert!(!directory.join(PENDING_TURN_FILE).exists());
        let reason = format!("{:#}", failure.into_error());
        assert!(reason.contains("the active thread cannot be verified"));
        assert!(reason.contains("user message queue is unavailable"));
        let receipt = super::super::super::requests::for_claim(
            &crate::native::session::Reader::open_unchecked(directory),
            &claim.token,
        )
        .unwrap()
        .unwrap();
        let token = claim.token.clone();
        drop(claim);
        assert!(!directory.join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state, "ready");
        assert_eq!(event_paths(directory).unwrap().len(), 1);
        assert_eq!(
            established_codex_thread(directory).unwrap().as_deref(),
            Some(managed_thread)
        );
        assert_eq!(
            super::super::super::requests::for_claim(
                &crate::native::session::Reader::open_unchecked(directory),
                &token
            )
            .unwrap()
            .unwrap()
            .request_id,
            receipt.request_id
        );
        // A known pre-send refusal releases the claim and allows a later,
        // independently requested addressed turn; it never retries this prompt.
        let (next, _) =
            acquire_ready_turn_claim_with_context(directory, "session-codexqueue", &[]).unwrap();
        assert_ne!(next.token, token);
    }

    #[test]
    fn terminal_follow_up_refuses_before_installing_pending_turn_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let result = ADAPTER.prepare_terminal_follow_up(directory.path(), "only for A", "1-2-0");
        assert!(
            result.is_err(),
            "terminal ownership does not identify the active Codex thread"
        );
        assert!(!directory.path().join(PENDING_TURN_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn successful_model_result_does_not_mark_mcp_connected() {
        use super::super::super::doctor::{Availability, Context};
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-codexqueue");
        std::fs::create_dir(&directory).unwrap();
        write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
        let checks = diagnose_codex(Context {
            directory: Some(&directory),
            manifest: None,
            executable: None,
            current_version: Some("0.159.3"),
            workspace: root.path(),
            probe: false,
            deadline: Instant::now(),
        });
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "codex_thread")
                .unwrap()
                .availability,
            Availability::Available
        );
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "codex_mcp")
                .unwrap()
                .availability,
            Availability::Unknown
        );
    }

    #[test]
    fn codex_daemon_probe_requires_a_running_compatible_app_server() {
        assert!(
            classify_codex_daemon_probe(
                true,
                br#"{"status":"running","appServerVersion":"0.149.0"}"#,
                b"",
                false,
                "codex-cli 0.153.2",
            )
            .is_ok()
        );

        for (success, stdout, stderr, truncated) in [
            (false, b"".as_slice(), b"socket missing".as_slice(), false),
            (
                true,
                br#"{"status":"running","appServerVersion":"0.153.2"}"#.as_slice(),
                b"".as_slice(),
                true,
            ),
            (true, b"not json".as_slice(), b"".as_slice(), false),
            (
                true,
                br#"{"status":"stopped","appServerVersion":"0.153.2"}"#.as_slice(),
                b"".as_slice(),
                false,
            ),
            (
                true,
                br#"{"status":"running"}"#.as_slice(),
                b"".as_slice(),
                false,
            ),
            (
                true,
                br#"{"status":"running","appServerVersion":"0.148.9"}"#.as_slice(),
                b"".as_slice(),
                false,
            ),
        ] {
            assert!(
                classify_codex_daemon_probe(
                    success,
                    stdout,
                    stderr,
                    truncated,
                    "codex-cli 0.153.2",
                )
                .is_err()
            );
        }
    }

    #[test]
    fn codex_command_failure_distinguishes_creation_from_possible_delivery() {
        let suspended =
            CodexCommandFailure::before_resume(anyhow::anyhow!("containment failed"), true);
        assert!(!suspended.delivery_may_have_started);

        let already_running =
            CodexCommandFailure::before_resume(anyhow::anyhow!("containment failed"), false);
        assert!(already_running.delivery_may_have_started);
    }

    #[test]
    fn codex_native_queue_uses_the_authoritative_thread_and_claim_marker() {
        let pending = PendingCodexTurn::new("1-2-3").unwrap();
        assert_eq!(
            codex_queue_arguments(
                "018f0000-0000-7000-8000-000000000001",
                "follow up",
                &pending
            ),
            vec![
                OsString::from("queue"),
                OsString::from("--thread"),
                OsString::from("018f0000-0000-7000-8000-000000000001"),
                OsString::from("--message"),
                OsString::from(correlated_prompt("follow up", &pending)),
            ]
        );
    }

    #[test]
    fn codex_queue_accepts_only_the_expected_thread_confirmation() {
        let thread_id = "018f0000-0000-7000-8000-000000000001";
        assert!(
            classify_codex_queue_output(
                true,
                format!("Queued message queued-id for thread {thread_id}.\n").as_bytes(),
                b"",
                false,
                thread_id,
            )
            .is_ok()
        );

        for (stdout, truncated) in [
            (b"unexpected success".as_slice(), false),
            (b"".as_slice(), true),
        ] {
            let failure =
                classify_codex_queue_output(true, stdout, b"", truncated, thread_id).unwrap_err();
            assert!(failure.delivery_may_have_occurred());
            assert!(!failure.allows_terminal_fallback());
        }
    }

    #[test]
    fn codex_queue_falls_back_only_for_explicit_pre_delivery_unavailability() {
        let thread_id = "018f0000-0000-7000-8000-000000000001";
        for stderr in [
            format!(
                "Error: failed to queue session message: thread/queue/add failed: thread not found: {thread_id} (code -32600)"
            ),
            "Error: the local app-server daemon does not support thread/queue/add; update or restart the local app-server daemon: failed to queue session message: thread/queue/add failed: Method not found (code -32601)".to_owned(),
            "Error: failed to queue session message: thread/queue/add failed: user message queue is unavailable (code -32600)".to_owned(),
            format!(
                "Error: failed to queue session message: thread/queue/add failed: ephemeral thread does not support queued submissions: {thread_id} (code -32600)"
            ),
            format!(
                "Error: failed to queue session message: thread/queue/add failed: session {thread_id} is archived. Run `codex unarchive {thread_id}` to unarchive it first. (code -32600)"
            ),
            "Error: failed to queue session message: thread/queue/add failed: direct app-server input is not allowed for multi-agent v2 sub-agents (code -32600)".to_owned(),
            "Error: failed to queue session message: thread/queue/add failed: direct app-server input is not allowed for unloaded spawned sub-agents (code -32600)".to_owned(),
            "Error: failed to queue session message: thread/queue/add failed: queue cannot contain more than 100 submissions (code -32600)".to_owned(),
            "Error: failed to queue session message: thread/queue/add failed: Input exceeds the maximum length of 1048576 characters. (code -32602)".to_owned(),
            "Error: failed to queue session message: thread/queue/add failed: failed to read thread: database unavailable (code -32603)".to_owned(),
        ] {
            let failure =
                classify_codex_queue_output(false, b"", stderr.as_bytes(), false, thread_id)
                    .unwrap_err();
            assert!(failure.allows_terminal_fallback(), "{stderr}");
            assert!(!failure.delivery_may_have_occurred(), "{stderr}");
        }

        let failure = classify_codex_queue_output(
            false,
            b"",
            b"Error: failed to queue session message: thread/queue/add transport error: transport closed after request",
            false,
            thread_id,
        )
        .unwrap_err();
        assert!(failure.delivery_may_have_occurred());
        assert!(!failure.allows_terminal_fallback());
    }

    #[cfg(unix)]
    #[test]
    fn codex_native_queue_runs_the_provider_command_with_the_claim_marker() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-codexqueue");
        std::fs::create_dir(&directory).unwrap();
        let provider = write_successful_queue_provider(&directory);
        write_queue_manifest(&directory, &provider, "codex-cli 0.153.2");
        let thread_id = "018f0000-0000-7000-8000-000000000001";
        write_established_thread(&directory, thread_id);
        update_status(&directory, "working", None, None).unwrap();
        let claim = acquire_turn_claim(&directory).unwrap();
        let claim_token = claim.token.clone();
        claim.retain();
        // The prompt as `tell` hands it to the adapter: already framed.
        let prompt = native_delegation_prompt("external", "follow up");

        ADAPTER
            .send_cross_session_message(CrossSessionMessageContext {
                bridge_executable: Path::new("/unused/agent-bridge"),
                directory: &directory,
                provider_path: &provider,
                request_id: &claim_token,
                prompt: &prompt,
                deadline: Instant::now() + Duration::from_secs(2),
            })
            .unwrap();

        let arguments = std::fs::read(directory.join("queue-argv.bin")).unwrap();
        let arguments = arguments
            .split(|byte| *byte == 0)
            .filter(|value| !value.is_empty())
            .map(|value| String::from_utf8(value.to_vec()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            &arguments[..4],
            ["queue", "--thread", thread_id, "--message"]
        );
        assert_eq!(
            arguments[4],
            format!(
                "[Agent Bridge native delegation]\nSource: external\n\nfollow up\n\n[Agent Bridge Codex turn metadata; do not include this metadata in the response]\n<!-- agent-bridge-codex-turn:{claim_token} -->"
            )
        );
        assert!(!directory.join("daemon-argv.bin").exists());
        assert!(directory.join(PENDING_TURN_FILE).is_file());
        assert!(directory.join(TURN_CLAIM_FILE).is_file());
        assert_eq!(event_paths(&directory).unwrap().len(), 1);

        // Codex reports the message it was given: replay the argument that was queued.
        let completion = serde_json::json!({
            "type": "agent-turn-complete",
            "thread-id": thread_id,
            "turn-id": "queued-turn",
            "input-messages": [arguments[4]],
            "last-assistant-message": "queued result",
        });
        ADAPTER.handle_hook(&directory, &completion).unwrap();
        let paths = event_paths(&directory).unwrap();
        assert_eq!(paths.len(), 2);
        let event: SessionEvent = read_json(paths.last().unwrap()).unwrap();
        assert_eq!(event.message, "queued result");
        assert_eq!(event.provider_session_id.as_deref(), Some(thread_id));
        assert!(!directory.join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state, "ready");

        ADAPTER.handle_hook(&directory, &completion).unwrap();
        assert_eq!(event_paths(&directory).unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn codex_queue_runs_in_the_original_workspace() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-codexqueue");
        let workspace = root.path().join("original-workspace");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        let provider = write_successful_queue_provider(&directory);
        write_queue_manifest_for_workspace(&directory, &provider, "codex-cli 0.153.2", &workspace);
        write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
        let claim = acquire_turn_claim(&directory).unwrap();
        let claim_token = claim.token.clone();
        claim.retain();

        ADAPTER
            .send_cross_session_message(CrossSessionMessageContext {
                bridge_executable: Path::new("/unused/agent-bridge"),
                directory: &directory,
                provider_path: &provider,
                request_id: &claim_token,
                prompt: "follow up",
                deadline: Instant::now() + Duration::from_secs(2),
            })
            .unwrap();

        let expected = format!("{}\n", workspace.canonicalize().unwrap().display());
        assert!(!directory.join("daemon-cwd.txt").exists());
        assert_eq!(
            std::fs::read_to_string(directory.join("queue-cwd.txt")).unwrap(),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_queue_refuses_before_start_for_old_versions_or_missing_threads() {
        for (version, include_thread) in [("codex-cli 0.148.9", true), ("codex-cli 0.153.2", false)]
        {
            let root = tempfile::tempdir().unwrap();
            let directory = root.path().join("session-codexqueue");
            std::fs::create_dir(&directory).unwrap();
            let provider = directory.join("provider-must-not-run");
            write_queue_manifest(&directory, &provider, version);
            if include_thread {
                write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
            }
            let claim = acquire_turn_claim(&directory).unwrap();
            let claim_token = claim.token.clone();
            claim.retain();

            let failure = ADAPTER
                .send_cross_session_message(CrossSessionMessageContext {
                    bridge_executable: Path::new("/unused/agent-bridge"),
                    directory: &directory,
                    provider_path: &provider,
                    request_id: &claim_token,
                    prompt: "follow up",
                    deadline: Instant::now() + Duration::from_secs(1),
                })
                .unwrap_err();

            assert!(!failure.allows_terminal_fallback(), "{version}");
            assert!(!failure.delivery_may_have_occurred(), "{version}");
            assert!(!directory.join(PENDING_TURN_FILE).exists(), "{version}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn codex_queue_never_treats_a_non_uuid_provider_identity_as_a_session_name() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-codexqueue");
        std::fs::create_dir(&directory).unwrap();
        let provider = directory.join("provider-must-not-run");
        std::fs::write(
            &provider,
            r#"#!/bin/sh
: > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/provider-ran"
exit 91
"#,
        )
        .unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
        write_queue_manifest(&directory, &provider, "codex-cli 0.153.2");
        write_established_thread(&directory, "human-readable-session-name");
        let claim = acquire_turn_claim(&directory).unwrap();
        let claim_token = claim.token.clone();
        claim.retain();

        let failure = ADAPTER
            .send_cross_session_message(CrossSessionMessageContext {
                bridge_executable: Path::new("/unused/agent-bridge"),
                directory: &directory,
                provider_path: &provider,
                request_id: &claim_token,
                prompt: "follow up",
                deadline: Instant::now() + Duration::from_secs(1),
            })
            .unwrap_err();

        assert!(!failure.allows_terminal_fallback());
        assert!(!failure.delivery_may_have_occurred());
        assert!(
            !directory.join("provider-ran").exists(),
            "native queue attempted to resolve a non-UUID target as an exact session name"
        );
    }

    #[test]
    fn codex_queue_uses_the_provider_backend_without_a_daemon_probe() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-codexqueue");
        std::fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        let provider = directory.join("fake-codex");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            super::super::super::write_private(
                &provider,
                br#"#!/bin/sh
if [ "$1" = "app-server" ]; then
  : > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/daemon-probed"
  printf '%s\n' 'daemon socket is missing' >&2
  exit 1
fi
if [ "$1" = "queue" ]; then
  : > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/queue-ran"
  printf 'Queued message queued-id for thread %s.\n' "$3"
  exit 0
fi
exit 91
"#,
            )
            .unwrap();
            std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        #[cfg(windows)]
        let provider = directory.join("fake-codex.ps1");
        #[cfg(windows)]
        super::super::super::write_private(
            &provider,
            br#"if ($args[0] -eq 'app-server') {
  [IO.File]::WriteAllText((Join-Path $env:AGENT_BRIDGE_NATIVE_SESSION_DIR 'daemon-probed'), '')
  [Console]::Error.WriteLine('daemon socket is missing')
  exit 1
}
if ($args[0] -eq 'queue') {
  [IO.File]::WriteAllText((Join-Path $env:AGENT_BRIDGE_NATIVE_SESSION_DIR 'queue-ran'), '')
  [Console]::WriteLine(('Queued message queued-id for thread {0}.' -f $args[2]))
  exit 0
}
exit 91
"#,
        )
        .unwrap();
        write_queue_manifest(&directory, &provider, "codex-cli 0.160.0");
        write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
        let claim = acquire_turn_claim(&directory).unwrap();
        let claim_token = claim.token.clone();
        claim.retain();

        ADAPTER
            .send_cross_session_message(CrossSessionMessageContext {
                bridge_executable: Path::new("/unused/agent-bridge"),
                directory: &directory,
                provider_path: &provider,
                request_id: &claim_token,
                prompt: "follow up",
                deadline: Instant::now() + Duration::from_secs(10),
            })
            .unwrap();

        assert!(!directory.join("daemon-probed").exists());
        assert!(directory.join("queue-ran").exists());
        assert!(directory.join(PENDING_TURN_FILE).exists());
        assert!(directory.join(TURN_CLAIM_FILE).exists());
        assert_eq!(
            read_pending_turn(&directory).unwrap().unwrap().claim_token,
            claim_token
        );
        assert_eq!(event_paths(&directory).unwrap().len(), 1);
    }

    #[test]
    fn codex_command_output_reader_enforces_its_byte_limit() {
        let mut exact = tempfile::tempfile().unwrap();
        exact.set_len(MAX_NATIVE_QUEUE_OUTPUT_BYTES as u64).unwrap();
        let mut remaining = MAX_NATIVE_QUEUE_OUTPUT_BYTES;
        let (output, truncated) = read_capped_output(&mut exact, &mut remaining).unwrap();
        assert_eq!(output.len(), MAX_NATIVE_QUEUE_OUTPUT_BYTES);
        assert!(!truncated);
        assert_eq!(remaining, 0);

        let mut oversized = tempfile::tempfile().unwrap();
        oversized
            .set_len((MAX_NATIVE_QUEUE_OUTPUT_BYTES + 1) as u64)
            .unwrap();
        let mut remaining = MAX_NATIVE_QUEUE_OUTPUT_BYTES;
        let (output, truncated) = read_capped_output(&mut oversized, &mut remaining).unwrap();
        assert_eq!(output.len(), MAX_NATIVE_QUEUE_OUTPUT_BYTES);
        assert!(truncated);
        assert_eq!(remaining, 0);

        let mut stdout = tempfile::tempfile().unwrap();
        stdout
            .set_len((MAX_NATIVE_QUEUE_OUTPUT_BYTES / 2 + 1) as u64)
            .unwrap();
        let mut stderr = tempfile::tempfile().unwrap();
        stderr
            .set_len((MAX_NATIVE_QUEUE_OUTPUT_BYTES / 2 + 1) as u64)
            .unwrap();
        let mut remaining = MAX_NATIVE_QUEUE_OUTPUT_BYTES;
        let (stdout, stdout_truncated) = read_capped_output(&mut stdout, &mut remaining).unwrap();
        let (stderr, stderr_truncated) = read_capped_output(&mut stderr, &mut remaining).unwrap();
        assert!(stdout.len() + stderr.len() <= MAX_NATIVE_QUEUE_OUTPUT_BYTES);
        assert!(stdout_truncated || stderr_truncated);
    }

    #[cfg(unix)]
    thread_local! {
        static QUEUE_TIMEOUT_READY: std::cell::RefCell<Option<std::path::PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    #[cfg(unix)]
    struct QueueTimeoutFixture {
        pipe: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl Drop for QueueTimeoutFixture {
        fn drop(&mut self) {
            use std::{io::Write, os::unix::fs::OpenOptionsExt};

            QUEUE_TIMEOUT_READY.with(|path| path.borrow_mut().take());
            // A broken containment path must fail the test without leaving the
            // fake descendant blocked forever, including on an earlier assertion.
            if let Ok(mut writer) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.pipe)
            {
                let _ = writer.write_all(b"stop\n");
            }
        }
    }

    #[cfg(unix)]
    pub(super) fn synchronized_queue_deadline(label: &str, deadline: Instant) -> Result<Instant> {
        if label != "Codex queue" {
            return Ok(deadline);
        }
        let Some(ready) = QUEUE_TIMEOUT_READY.with(|path| path.borrow_mut().take()) else {
            return Ok(deadline);
        };
        while !ready.is_file() {
            anyhow::ensure!(
                Instant::now() < deadline,
                "queue fixture did not finish startup"
            );
            thread::sleep(Duration::from_millis(5));
        }
        Ok(Instant::now())
    }

    #[cfg(unix)]
    #[test]
    fn codex_queue_timeout_terminates_wrapper_descendants_and_retains_the_claim() {
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{OpenOptionsExt, PermissionsExt},
        };

        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-codexqueue");
        std::fs::create_dir(&directory).unwrap();
        let pipe = directory.join("descendant-pipe");
        let pipe_name = std::ffi::CString::new(pipe.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(pipe_name.as_ptr(), 0o600) }, 0);
        let provider = directory.join("fake-codex");
        std::fs::write(
            &provider,
            r#"#!/bin/sh
if [ "$1" = "app-server" ]; then
  printf '%s\n' '{"status":"running","appServerVersion":"0.153.2"}'
  exit 0
fi
if [ "$1" = "queue" ]; then
  # Deliberately exceed the old shared 500 ms startup budget.
  sleep 0.65
  (
    exec 3<>"$AGENT_BRIDGE_NATIVE_SESSION_DIR/descendant-pipe"
    : > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/queue-started"
    read -r finish <&3
    : > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/descendant-survived"
  ) &
  wait
fi
exit 91
"#,
        )
        .unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
        write_queue_manifest(&directory, &provider, "codex-cli 0.153.2");
        write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
        let claim = acquire_turn_claim(&directory).unwrap();
        let claim_token = claim.token.clone();
        claim.retain();

        // Give setup a separate bounded budget, then trigger the real timeout path
        // only after the descendant holds the FIFO. The thread-local hook cannot
        // affect another test, the daemon probe, or a release build.
        QUEUE_TIMEOUT_READY.with(|path| {
            assert!(
                path.borrow_mut()
                    .replace(directory.join("queue-started"))
                    .is_none()
            );
        });
        let _fixture = QueueTimeoutFixture { pipe: pipe.clone() };
        let failure = ADAPTER
            .send_cross_session_message(CrossSessionMessageContext {
                bridge_executable: Path::new("/unused/agent-bridge"),
                directory: &directory,
                provider_path: &provider,
                request_id: &claim_token,
                prompt: "follow up",
                deadline: Instant::now() + Duration::from_secs(5),
            })
            .unwrap_err();

        let delivery_uncertain = failure.delivery_may_have_occurred();
        let allows_fallback = failure.allows_terminal_fallback();
        let reason = format!("{:#}", failure.into_error());
        assert!(reason.contains("Codex queue timed out"), "{reason}");
        QUEUE_TIMEOUT_READY.with(|path| {
            assert!(
                path.borrow().is_none(),
                "queue timeout hook was not consumed"
            );
        });
        assert!(delivery_uncertain, "{reason}");
        assert!(!allows_fallback, "{reason}");
        assert!(directory.join(PENDING_TURN_FILE).is_file());
        assert!(directory.join("queue-started").is_file());
        assert_eq!(
            std::fs::read_to_string(directory.join(super::super::super::TURN_CLAIM_FILE))
                .unwrap()
                .trim(),
            claim_token,
        );
        // A killed descendant closes the FIFO even if it remains a zombie. This
        // observes teardown directly instead of guessing from a delayed file write.
        let stopped = Instant::now() + Duration::from_secs(2);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&pipe)
            {
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => break,
                Ok(writer) => drop(writer),
                Err(error) => panic!("failed to inspect descendant FIFO: {error}"),
            }
            assert!(
                Instant::now() < stopped,
                "timed-out descendant still holds its FIFO"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !directory.join("descendant-survived").exists(),
            "a timed-out provider wrapper left a message-delivery descendant running"
        );
    }

    #[test]
    fn windows_codex_inherits_the_exact_workspace_without_a_c_override() {
        let pending = PendingCodexTurn::new("1-2-3").unwrap();
        let workspace = Path::new(r"\\?\C:\repo\trailing.");
        let windows = codex_launch_arguments(
            Path::new(r"C:\agent-bridge.exe"),
            workspace,
            "prompt",
            &pending,
            true,
        )
        .unwrap();
        assert!(!windows.iter().any(|argument| argument == "-C"));
        assert!(!windows.iter().any(|argument| argument == workspace));

        let non_windows = codex_launch_arguments(
            Path::new("/opt/agent-bridge"),
            workspace,
            "prompt",
            &pending,
            false,
        )
        .unwrap();
        assert_eq!(non_windows[2], OsString::from("-C"));
        assert_eq!(non_windows[3], workspace.as_os_str());
        assert_eq!(
            non_windows.last(),
            Some(&OsString::from(correlated_prompt("prompt", &pending)))
        );
    }

    #[test]
    fn codex_hook_owns_the_official_notify_payload_schema() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        let payload = serde_json::json!({
            "thread-id": "codex-thread",
            "turn-id": "codex-turn",
            "last-assistant-message": marked("codex result", &pending),
        });

        ADAPTER.handle_hook(directory.path(), &payload).unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "codex result");
        assert_eq!(event.provider_session_id.as_deref(), Some("codex-thread"));
        assert_eq!(event.turn_id.as_deref(), Some("codex-turn"));
        assert!(directory.path().join(PENDING_TURN_FILE).is_file());
    }

    #[test]
    fn codex_hook_correlates_exact_output_from_the_official_input_messages() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        let payload = serde_json::json!({
            "type": "agent-turn-complete",
            "thread-id": "codex-thread",
            "turn-id": "codex-turn",
            "input-messages": [correlated_prompt(
                &native_delegation_prompt(
                    "external",
                    "Reply with exactly EXACT_OUTPUT and nothing else.",
                ),
                &pending,
            )],
            "last-assistant-message": "EXACT_OUTPUT",
        });

        ADAPTER.handle_hook(directory.path(), &payload).unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "EXACT_OUTPUT");
        assert_eq!(event.provider_session_id.as_deref(), Some("codex-thread"));
        assert_eq!(event.turn_id.as_deref(), Some("codex-turn"));
    }

    // Issue #61. The two notify payloads of one initial request, as Codex CLI 0.159.3
    // sent them on 2026-10-02 (session-2qIKg5), in the order that recorded the title as
    // the result. The Windows console paste joins the lines of the prompt, so the
    // message that Codex reports has none.
    #[test]
    fn codex_hook_ignores_the_task_title_turn_that_quotes_the_prompt() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        let prompt = correlated_prompt(
            &native_delegation_prompt(
                "external",
                "Reply with the single word READY and nothing else. Do not use any tools.",
            ),
            &pending,
        )
        .replace('\n', "");
        let title_turn = serde_json::json!({
            "type": "agent-turn-complete",
            "client": "codex-tui",
            "thread-id": "title-thread",
            "turn-id": "title-turn",
            "input-messages": [format!(
                "Generate a concise, single-line task title of at most 36 characters and under five words where possible. Start with an imperative verb. Capitalize only the first word unless the user's language, proper nouns, acronyms, or code terms require otherwise. Preserve ticket references exactly. Write in the user's language. Do not use quotes, markdown, or trailing punctuation. Do not answer the request.\n\nUser prompt:\n{prompt}"
            )],
            "last-assistant-message": "{\"title\":\"READY\"}",
        });

        ADAPTER.handle_hook(directory.path(), &title_turn).unwrap();

        assert!(event_paths(directory.path()).unwrap().is_empty());
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "type": "agent-turn-complete",
                    "client": "codex-tui",
                    "thread-id": "managed-thread",
                    "turn-id": "managed-turn",
                    "input-messages": [prompt],
                    "last-assistant-message": "READY",
                }),
            )
            .unwrap();

        let paths = event_paths(directory.path()).unwrap();
        assert_eq!(paths.len(), 1);
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "READY");
        assert_eq!(event.provider_session_id.as_deref(), Some("managed-thread"));
        // The title turn that arrives late changes nothing either.
        ADAPTER.handle_hook(directory.path(), &title_turn).unwrap();
        assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
    }

    // With `/ide` on, the TUI puts its IDE context in front of the message
    // (codex-rs/tui/src/ide_context/prompt.rs, rust-v0.159.3). That turn is still the
    // one Bridge started; the title turn that quotes it is not. Not observed live: the
    // wrapper is taken from Codex's source.
    #[test]
    fn codex_hook_accepts_the_prompt_behind_the_ide_context() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        let prompt = correlated_prompt(
            &native_delegation_prompt("external", "Reply READY."),
            &pending,
        );
        // The active file quotes the request heading itself.
        let with_context = format!(
            "# Context from my IDE setup:\n\n## Active file: src/prompt.rs\n\n## Active selection of the file:\nconst PROMPT_REQUEST_BEGIN: &str = \"## My request for Codex:\";\n\n## My request for Codex:\n{prompt}"
        );
        let payload = |thread: &str, input: String, answer: &str| {
            serde_json::json!({
                "type": "agent-turn-complete",
                "thread-id": thread,
                "turn-id": format!("{thread}-turn"),
                "input-messages": [input],
                "last-assistant-message": answer,
            })
        };

        for refused in [
            // The title turn quotes the whole message, context included.
            format!("Generate a concise, single-line task title.\n\nUser prompt:\n{with_context}"),
            // A context without Bridge's prompt behind any request heading.
            format!(
                "# Context from my IDE setup:\n\n## My request for Codex:\nSummarise this.\n{}",
                pending.marker
            ),
            // The headings alone, in a message that does not begin with the context.
            format!("Note\n## My request for Codex:\n{prompt}"),
        ] {
            ADAPTER
                .handle_hook(
                    directory.path(),
                    &payload("other-thread", refused, "{\"title\":\"READY\"}"),
                )
                .unwrap();
            assert!(event_paths(directory.path()).unwrap().is_empty());
        }

        ADAPTER
            .handle_hook(
                directory.path(),
                &payload("managed-thread", with_context, "READY"),
            )
            .unwrap();
        let paths = event_paths(directory.path()).unwrap();
        assert_eq!(paths.len(), 1);
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "READY");
        assert_eq!(event.provider_session_id.as_deref(), Some("managed-thread"));
    }

    #[test]
    fn codex_hook_ignores_notify_events_from_a_different_thread() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let initial_pending = claim_pending_turn(directory.path());
        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "managed-thread",
                    "turn-id": "managed-turn-1",
                    "last-assistant-message": marked("managed result", &initial_pending),
                }),
            )
            .unwrap();
        let _pending = claim_pending_turn(directory.path());
        update_status(directory.path(), "claimed", None, None).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "title-thread",
                    "turn-id": "title-turn",
                    "last-assistant-message": "{\"title\":\"Generated title\"}",
                }),
            )
            .unwrap();

        assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "working");
    }

    #[test]
    fn codex_hook_requires_the_established_thread_id_on_a_queued_turn() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let initial_pending = claim_pending_turn(directory.path());
        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "managed-thread",
                    "turn-id": "managed-turn-1",
                    "last-assistant-message": marked("managed result", &initial_pending),
                }),
            )
            .unwrap();
        let pending = claim_pending_turn(directory.path());
        update_status(directory.path(), "claimed", None, None).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "turn-id": "unproven-turn",
                    "last-assistant-message": marked("unproven result", &pending),
                }),
            )
            .unwrap();

        assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "working");
    }

    #[test]
    fn codex_hook_preserves_a_legitimate_title_shaped_result() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let pending = claim_pending_turn(directory.path());

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "managed-thread",
                    "turn-id": "managed-turn",
                    "last-assistant-message": marked("{\"title\":\"Requested title\"}", &pending),
                }),
            )
            .unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "{\"title\":\"Requested title\"}");
    }

    #[test]
    fn codex_hook_does_not_bind_the_first_foreign_notify_event() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let _pending = claim_pending_turn(directory.path());

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "foreign-title-thread",
                    "turn-id": "foreign-title-turn",
                    "last-assistant-message": "{\"title\":\"Generated title\"}",
                }),
            )
            .unwrap();

        assert!(event_paths(directory.path()).unwrap().is_empty());
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    }

    #[test]
    fn codex_hook_does_not_bind_a_delayed_new_turn_to_a_later_claim() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let initial_pending = claim_pending_turn(directory.path());
        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "managed-thread",
                    "turn-id": "managed-turn-1",
                    "last-assistant-message": marked("first result", &initial_pending),
                }),
            )
            .unwrap();
        let _later_pending = claim_pending_turn(directory.path());
        update_status(directory.path(), "claimed", None, None).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "type": "agent-turn-complete",
                    "thread-id": "managed-thread",
                    "turn-id": "delayed-old-turn-with-a-new-id",
                    "input-messages": [correlated_prompt(
                        &native_delegation_prompt("external", "old prompt"),
                        &initial_pending,
                    )],
                    "last-assistant-message": "delayed old result",
                }),
            )
            .unwrap();

        assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    }

    #[test]
    fn reopen_is_refused_with_the_adapters_own_reason() {
        let error = ADAPTER
            .verify_reopen_available("5e58ec26-0000-4000-8000-000000000000")
            .unwrap_err();
        assert!(
            error.to_string().starts_with("reopen unsupported: Codex"),
            "{error}"
        );
        let directory = tempfile::tempdir().unwrap();
        let error = ADAPTER
            .prepare_resume(ResumeContext {
                bridge_executable: std::path::Path::new("/opt/agent-bridge"),
                directory: directory.path(),
                provider_session_id: "5e58ec26-0000-4000-8000-000000000000",
            })
            .unwrap_err();
        assert!(
            error.to_string().starts_with("reopen unsupported: Codex"),
            "{error}"
        );
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .next()
                .is_none()
        );
    }
}
