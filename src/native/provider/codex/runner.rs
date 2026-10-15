use anyhow::{Context, Result};
use std::{
    io::{Read, Seek, SeekFrom},
    process::{ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAX_NATIVE_QUEUE_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub(super) struct BoundedCommandOutput {
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) truncated: bool,
}

#[derive(Debug)]
pub(super) struct CodexCommandFailure {
    pub(super) error: anyhow::Error,
    pub(super) delivery_may_have_started: bool,
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

pub(super) fn run_bounded_command_until(
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
    crate::native::provider_process::configure_process_tree(command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_sink))
        .stderr(Stdio::from(stderr_sink))
        .spawn()
        .with_context(|| format!("failed to start {label}"))
        .map_err(CodexCommandFailure::not_started)?;
    let process_tree = match crate::native::provider_process::ProviderProcessTree::attach(&child) {
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
    let deadline = match super::test_support::synchronized_queue_deadline(label, deadline) {
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
    process_tree: &crate::native::provider_process::ProviderProcessTree,
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

#[cfg(test)]
mod tests;
