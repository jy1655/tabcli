use super::correlation::{correlated_prompt, established_codex_thread, valid_codex_thread_id};
use super::runner::run_bounded_command_until;
use super::{
    PendingCodexTurn, cancel_pending_turn, codex_version_supports_native_queue, write_pending_turn,
};
use crate::native::clock::Clock;
use crate::native::delivery::Outcome;
use crate::native::failure_kind::Unsupported;
use crate::native::provider::CrossSessionMessageContext;
use crate::native::session::Reader;
use agent_bridge::PUBLIC_COMMAND;
use anyhow::Result;
use std::{ffi::OsString, path::Path};

// Refuse terminal follow-up without an atomic, provider-owned active-thread input path.
// A title, old notify or live process cannot prove which thread receives input.
fn unaddressed_follow_up() -> String {
    format!(
        "Codex terminal follow-up is unavailable: the active thread cannot be verified; no terminal input was sent. Use the thread-addressed native queue with Codex 0.149+; inspect the queue error and `{PUBLIC_COMMAND} doctor <session> --probe` for the unavailable prerequisite"
    )
}

fn queue_command(
    executable: &Path,
    directory: &Path,
    workspace: &Path,
    arguments: Vec<OsString>,
) -> Result<std::process::Command> {
    let mut command =
        crate::native::process_env::provider_command(executable, directory, arguments, &[])?;
    command.current_dir(workspace).env(
        crate::native::session::primitives::SESSION_DIR_ENV,
        directory,
    );
    Ok(command)
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

fn queue_unavailable(error: anyhow::Error) -> Outcome {
    Outcome::NotSent(error.context(unaddressed_follow_up()))
}

pub(super) fn send_native_queue_message<C: Clock>(
    context: CrossSessionMessageContext<'_>,
    clock: &mut C,
) -> std::result::Result<(), Outcome> {
    let manifest = Reader::open_unchecked(context.directory)
        .manifest()
        .map_err(Outcome::NotSent)?;
    match codex_version_supports_native_queue(&manifest.provider_version) {
        Ok(true) => {}
        Ok(false) => {
            // Only the version floor is a capability gate; the shared context stays text.
            return Err(queue_unavailable(anyhow::Error::new(Unsupported::new(
                format!(
                    "Codex {} predates native queue support in 0.149.0",
                    manifest.provider_version
                ),
            ))));
        }
        Err(error) => return Err(Outcome::NotSent(error)),
    }
    let thread_id = established_codex_thread(context.directory)
        .map_err(Outcome::NotSent)?
        .ok_or_else(|| {
            queue_unavailable(anyhow::anyhow!(
                "Codex session has no established provider thread id"
            ))
        })?;
    if !valid_codex_thread_id(&thread_id) {
        return Err(queue_unavailable(anyhow::anyhow!(
            "Codex session provider identity is not a thread UUID"
        )));
    }
    // Codex chooses daemon or embedded queue; acceptance is not completion, which needs notify.
    // See docs/verification/2026-10-04-codex-queue.md for the provider source.

    let mut pending = PendingCodexTurn::new(context.correlation_id).map_err(Outcome::NotSent)?;
    pending.record_framed_prompt(&correlated_prompt(context.prompt, &pending));
    let arguments = codex_queue_arguments(&thread_id, context.prompt, &pending);
    let mut command = queue_command(
        context.provider_path,
        context.directory,
        &manifest.workspace,
        arguments,
    )
    .map_err(|error| {
        queue_unavailable(error.context("failed to prepare the Codex queue command"))
    })?;
    #[cfg(windows)]
    {
        use crate::native::provider_process::{
            WINDOWS_COMMAND_LINE_LIMIT, windows_command_line_units,
        };
        let units = windows_command_line_units(&command);
        if units >= WINDOWS_COMMAND_LINE_LIMIT {
            return Err(queue_unavailable(anyhow::anyhow!(
                "Codex queue command line measures {units} UTF-16 units; the Windows limit is {WINDOWS_COMMAND_LINE_LIMIT} units (command line must be shorter). Shorten the follow-up and pass long material by path in the workspace; nothing was sent"
            )));
        }
    }
    write_pending_turn(context.directory, &pending).map_err(Outcome::NotSent)?;
    let result =
        match run_bounded_command_until(&mut command, context.deadline, "Codex queue", clock) {
            Ok(output) => classify_codex_queue_output(
                output.status.success(),
                &output.stdout,
                &output.stderr,
                output.truncated,
                &thread_id,
            ),
            Err(failure) if !failure.delivery_may_have_started => {
                Err(queue_unavailable(failure.error))
            }
            Err(failure) => Err(Outcome::Uncertain(failure.error)),
        };
    if result
        .as_ref()
        .is_err_and(|failure| matches!(failure, Outcome::NotSent(_)))
    {
        let _ = cancel_pending_turn(context.directory, context.correlation_id);
    }
    result
}

fn classify_codex_queue_output(
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
    output_truncated: bool,
    thread_id: &str,
) -> std::result::Result<(), Outcome> {
    if output_truncated {
        return Err(Outcome::Uncertain(anyhow::anyhow!(
            "Codex queue output exceeded the safety limit"
        )));
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
        return Err(Outcome::Uncertain(anyhow::anyhow!(
            "Codex queue reported success without the expected thread confirmation"
        )));
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
        Err(queue_unavailable(error))
    } else {
        Err(Outcome::Uncertain(error))
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

#[cfg(test)]
mod tests;
