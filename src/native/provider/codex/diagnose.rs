use super::correlation::{correlated_prompt, established_codex_thread, valid_codex_thread_id};
use super::{ADAPTER, codex_version_supports_native_queue, read_pending_turn};
use crate::native::provider::NativeProviderAdapter;
use crate::native::session::{CoreRecord, Reader, requests};
use anyhow::{Context, Result, bail};
use std::path::Path;

mod hook_trust;
mod lifecycle;

/// Windows limits a process command line to this many UTF-16 units (`CreateProcessW`).
/// Codex runs the configured `notify` command with the whole payload as one argument.
pub(super) const NOTIFY_COMMAND_LINE_LIMIT_UNITS: usize = 32_767;
/// Below this remaining budget for the answer the check reports `near_limit`.
pub(super) const NOTIFY_ANSWER_BUDGET_THRESHOLD_UNITS: usize = 4_096;
/// Allowance for the payload's other fields (`type`, `thread-id`, `turn-id`, the JSON
/// framing of `input-messages` and `last-assistant-message`) and the notify command's own
/// arguments (`native-hook codex`) and quoting, on top of the executable path. Not a proven
/// bound: the real command line can be larger or smaller than the estimate, and Codex's
/// argument quoting adds a backslash for every quotation mark in the payload, so a prompt
/// or an answer with many quotation marks is undercounted by that many units.
const NOTIFY_ENVELOPE_FIXED_UNITS: usize = 300;

/// The command-line budget of one Codex `notify` payload, estimated from the framed prompt.
/// An estimate, not a bound: the envelope is an allowance, and the executable length is
/// the doctor's own path standing in for the notify path fixed at launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NotifyPayloadBudget {
    pub(super) prompt_chars: usize,
    pub(super) escaped_units: usize,
    pub(super) envelope_units: usize,
    pub(super) limit_units: usize,
    pub(super) answer_budget_units: usize,
}

/// The sizes the pending-turn record keeps of a framed prompt: its characters, and the
/// UTF-16 units of its JSON escape, which is how the notify payload carries it.
pub(super) fn framed_prompt_sizes(framed: &str) -> (u64, u64) {
    let escaped_units = serde_json::to_string(framed)
        .map(|escaped| escaped.encode_utf16().count())
        .unwrap_or(0);
    (framed.chars().count() as u64, escaped_units as u64)
}

pub(super) fn notify_payload_budget(prompt: &str, executable_units: usize) -> NotifyPayloadBudget {
    let (prompt_chars, escaped_units) = framed_prompt_sizes(prompt);
    notify_payload_budget_from_sizes(prompt_chars, escaped_units, executable_units)
}

pub(super) fn notify_payload_budget_from_sizes(
    prompt_chars: u64,
    escaped_units: u64,
    executable_units: usize,
) -> NotifyPayloadBudget {
    let escaped_units = usize::try_from(escaped_units).unwrap_or(usize::MAX);
    let envelope_units = executable_units + NOTIFY_ENVELOPE_FIXED_UNITS;
    NotifyPayloadBudget {
        prompt_chars: usize::try_from(prompt_chars).unwrap_or(usize::MAX),
        escaped_units,
        envelope_units,
        limit_units: NOTIFY_COMMAND_LINE_LIMIT_UNITS,
        answer_budget_units: NOTIFY_COMMAND_LINE_LIMIT_UNITS
            .saturating_sub(escaped_units.saturating_add(envelope_units)),
    }
}

pub(super) fn classify_notify_payload_budget(
    budget: &NotifyPayloadBudget,
    windows: bool,
) -> (crate::native::diagnostics::Availability, &'static str) {
    use crate::native::diagnostics::Availability::{Available, Unavailable};
    if !windows {
        (Available, "codex_notify_payload_no_limit")
    } else if budget.escaped_units.saturating_add(budget.envelope_units) >= budget.limit_units {
        (Unavailable, "codex_notify_payload_over_limit")
    } else if budget.answer_budget_units < NOTIFY_ANSWER_BUDGET_THRESHOLD_UNITS {
        (Unavailable, "codex_notify_payload_near_limit")
    } else {
        (Available, "codex_notify_payload_within_limit")
    }
}

fn notify_payload_evidence(
    budget: &NotifyPayloadBudget,
    pending_request: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "prompt_chars": budget.prompt_chars,
        "escaped_units": budget.escaped_units,
        "envelope_units": budget.envelope_units,
        "limit_units": budget.limit_units,
        "answer_budget_units": budget.answer_budget_units,
        "threshold_units": NOTIFY_ANSWER_BUDGET_THRESHOLD_UNITS,
        "executable_source": "doctor_executable",
        "pending_request": pending_request,
    })
}

const NOTIFY_PAYLOAD_DETAIL: &str = "This request may still depend on notify (no binding observed or lifecycle hooks were not recorded as injected). Estimate of the pending turn's notify command line, not a measurement: the JSON-escaped framed prompt in UTF-16 units, as sized before delivery, plus an allowance of 300 units for the payload's other fields and Codex's argument quoting, plus the length of this doctor's executable path standing in for the notify executable fixed at launch, which the records do not retain. The real command line can be larger or smaller: Codex's argument quoting adds a backslash for every quotation mark in the payload, so text with many quotation marks is undercounted. Windows refuses a command line of 32,767 units or more: an estimate at or over the limit means the completion has probably failed to reach Bridge, and one near the limit that a long answer will.";
const NOTIFY_PAYLOAD_ACTION: &str = "If the request stays pending while the session is working after a long turn, read the answer from Codex's rollout record (~/.codex/sessions/<date>/rollout-*.jsonl) and close-session the session; never resend an uncertain turn. Otherwise keep delivered prompts and requested answers short on native Windows: put long material in workspace files and ask for a file-based report.";

fn notify_payload_check(directory: Option<&Path>) -> crate::native::diagnostics::Check {
    use crate::native::diagnostics::{
        Availability::{Available, Unknown},
        Check,
    };
    let Some(directory) = directory else {
        return Check::new(
            "codex_notify_payload_budget",
            Unknown,
            "session_required",
            NOTIFY_PAYLOAD_DETAIL,
            NOTIFY_PAYLOAD_ACTION,
        );
    };
    let pending = match read_pending_turn(directory) {
        Ok(Some(pending)) => pending,
        Ok(None) => {
            return Check::new(
                "codex_notify_payload_budget",
                Unknown,
                "codex_turn_not_pending",
                "No Codex turn is pending in this session, so there is no payload to estimate.",
                NOTIFY_PAYLOAD_ACTION,
            );
        }
        Err(error) => {
            return Check::new(
                "codex_notify_payload_budget",
                Unknown,
                "codex_pending_prompt_unavailable",
                NOTIFY_PAYLOAD_DETAIL,
                NOTIFY_PAYLOAD_ACTION,
            )
            .evidence(serde_json::json!({"error": format!("{error:#}")}));
        }
    };
    if matches!(super::hooks::read_launch_mode(directory), Ok(Some(mode)) if mode.lifecycle_hooks == super::hooks::LifecycleHooks::Injected)
        && matches!(
            super::turn::binding(&Reader::open_unchecked(directory), &pending.claim_token),
            Ok(Some(_))
        )
    {
        return Check::new(
            "codex_notify_payload_budget",
            Available,
            "codex_notify_payload_not_required",
            "This session recorded hook injection and a binding for this request; notify is not its terminal authority, even if transcript observation is suspended.",
            "Inspect codex_lifecycle for this request's recorded observer state.",
        );
    }
    let pending_request = Reader::open_unchecked(directory)
        .record(CoreRecord::Requests)
        .child(&format!("{}.json", pending.claim_token))
        .json::<requests::Receipt>()
        .ok()
        .map(|receipt| receipt.request_id);
    let executable_units = std::env::current_exe()
        .map(|path| path.to_string_lossy().encode_utf16().count())
        .unwrap_or(0);
    let budget = match (pending.framed_prompt_chars, pending.framed_prompt_units) {
        // The sizes the install wrote into the record: the framed prompt before delivery.
        (Some(chars), Some(units)) => Ok((
            notify_payload_budget_from_sizes(chars, units, executable_units),
            "pending_turn_record",
        )),
        // A record without them was installed by an older binary; the preserved initial
        // prompt still describes the turn while it exists.
        _ => pending_prompt(directory, &pending).map(|prompt| {
            (
                notify_payload_budget(&prompt, executable_units),
                "initial_prompt_record",
            )
        }),
    };
    match budget {
        Ok((budget, source)) => {
            let (availability, reason) = classify_notify_payload_budget(&budget, cfg!(windows));
            let mut evidence = notify_payload_evidence(&budget, pending_request.as_deref());
            evidence["source"] = serde_json::Value::from(source);
            Check::new(
                "codex_notify_payload_budget",
                availability,
                reason,
                NOTIFY_PAYLOAD_DETAIL,
                NOTIFY_PAYLOAD_ACTION,
            )
            .evidence(evidence)
        }
        Err(error) => Check::new(
            "codex_notify_payload_budget",
            Unknown,
            "codex_pending_prompt_unavailable",
            NOTIFY_PAYLOAD_DETAIL,
            NOTIFY_PAYLOAD_ACTION,
        )
        .evidence(serde_json::json!({
            "error": format!("{error:#}"),
            "pending_request": pending_request,
        })),
    }
}

/// The fallback for a pending turn whose record carries no sizes: the preserved initial
/// prompt, framed as Codex received it. The wrapper removes that record when the launch
/// argument accepts it (macOS) and delivery removes it after the paste (Windows), and
/// `tell` keeps no copy of a follow-up prompt, so a turn installed before the sizes were
/// recorded is only estimable before its delivery.
fn pending_prompt(directory: &Path, pending: &super::PendingCodexTurn) -> Result<String> {
    let prompt = Reader::open_unchecked(directory)
        .initial_prompt()
        .context("the pending turn record of an older version carries no prompt sizes and the initial prompt record is gone: the delivered prompt is not retained")?;
    Ok(correlated_prompt(&prompt, pending))
}

pub(super) fn diagnose_codex(
    context: crate::native::diagnostics::Context<'_>,
) -> Vec<crate::native::diagnostics::Check> {
    use crate::native::diagnostics::{
        Availability::{Available, Unavailable, Unknown},
        Check,
    };
    use crate::native::doctor;
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
    checks.push(notify_payload_check(context.directory));
    checks.push(lifecycle::check(context.directory));
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
    // Existing probes consume their shared budget first. The hook exchange owns
    // its explicitly documented ten-second bound, without starving those probes.
    if context.probe {
        checks.push(hook_trust::check(&context));
    }
    checks
}

fn diagnose_daemon_output(
    output: &std::process::Output,
    version: &str,
) -> crate::native::diagnostics::Check {
    use crate::native::diagnostics::{
        Availability::{Available, Unavailable, Unknown},
        Check,
    };
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

#[cfg(test)]
mod tests;
