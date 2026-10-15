use super::correlation::{established_codex_thread, valid_codex_thread_id};
use super::{ADAPTER, codex_version_supports_native_queue};
use crate::native::provider::NativeProviderAdapter;
use anyhow::{Context, Result, bail};

pub(super) fn diagnose_codex(
    context: crate::native::doctor::Context<'_>,
) -> Vec<crate::native::doctor::Check> {
    use crate::native::doctor::{self, Availability::*, Check};
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
) -> crate::native::doctor::Check {
    use crate::native::doctor::{Availability::*, Check};
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
