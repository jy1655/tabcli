//! Process authority for an explicit close after the bound Herdr core reports absence.
use super::NativeSessionOwner;
use crate::native::{
    session::{Reader, close::budget::close_remaining},
    terminal::{CloseOutcome, CloseResult, WindowsProcessIdentity},
};
use anyhow::{Context, Result, ensure};
use std::time::Instant;

/// Both operations use the kernel handle that proved the recorded incarnation.
pub(in crate::native::terminal) trait PinnedProcess {
    fn exited(&self) -> Result<bool>;
    /// True for a confirmed termination, false for a process that exited independently.
    fn stop(&self, deadline: Instant) -> Result<bool>;
}

fn provider(reader: &Reader, session: &str, wrapper: u32) -> Result<(u32, WindowsProcessIdentity)> {
    let record = reader
        .provider_process_if_present()?
        .context("the provider process record is missing")?;
    ensure!(
        record.schema == 1 && record.managed_session_id == session,
        "the provider process record is unsupported or belongs to another session"
    );
    ensure!(
        record.pid != 0 && record.pid != wrapper,
        "the provider process record has an invalid provider PID"
    );
    let identity = record
        .windows_process_identity
        .context("the recorded provider has no Windows identity")?;
    Ok((record.pid, identity))
}

pub(super) fn teardown<H: PinnedProcess>(
    reader: &Reader,
    owner: &NativeSessionOwner,
    present: impl FnOnce() -> Result<bool>,
    mut pin: impl FnMut(u32, &WindowsProcessIdentity) -> Result<Option<H>>,
    deadline: Instant,
) -> Result<CloseResult> {
    close_remaining(deadline)?;
    let session = owner
        .managed_session_id
        .as_deref()
        .context("missing Herdr owner binding")?;
    let pid = owner.pid;
    let identity = owner
        .windows_process_identity
        .as_ref()
        .context("missing Herdr owner identity")?;
    let wrapper = pin(pid, identity)?
        .with_context(|| format!("recorded wrapper PID {pid} has exited; retry close"))?;
    let selected = provider(reader, session, pid).and_then(|(pid, identity)| {
        let process = pin(pid, &identity)
            .with_context(|| format!("could not verify recorded provider PID {pid}"))?;
        Ok((pid, process, identity))
    });

    // The caller holds the session lifecycle lock across both pins and the last evidence read.
    ensure!(
        !present()?,
        "the recorded Herdr terminal is present; the handle is retained"
    );
    ensure!(
        reader.owner()? == *owner,
        "the recorded Herdr owner changed; the handle is retained"
    );
    ensure!(
        !wrapper.exited()?,
        "the recorded Herdr wrapper exited; retry close"
    );
    let verified = selected.and_then(|(provider_pid, handle, identity)| {
        ensure!(
            provider(reader, session, pid)? == (provider_pid, identity),
            "the recorded provider identity changed"
        );
        Ok((provider_pid, handle))
    });
    close_remaining(deadline)?;
    let provider_outcome = match verified {
        Ok((provider_pid, process)) => {
            let stopped = process.as_ref().map(|process| process.stop(deadline)).transpose()
                .with_context(|| format!("could not confirm exit of recorded provider PID {provider_pid}; the handle is retained"))?;
            if stopped == Some(true) {
                format!("recorded provider PID {provider_pid} was already ended by TAB")
            } else {
                format!("recorded provider PID {provider_pid} had already exited")
            }
        }
        Err(error) => format!("no provider process was ended by TAB: {error:#}"),
    };
    let stopped = close_remaining(deadline).and_then(|_| wrapper.stop(deadline))
        .with_context(|| format!("{provider_outcome}; could not confirm exit of recorded wrapper PID {pid}; the handle is retained"))?;
    let wrapper_outcome = if stopped {
        "was ended by TAB"
    } else {
        "had already exited"
    };
    Ok(CloseResult {
        reason: Some(format!(
            "Herdr terminal absent; {provider_outcome}; recorded wrapper PID {pid} {wrapper_outcome}"
        )),
        ..CloseOutcome::Missing.into()
    })
}

#[cfg(test)]
mod tests;
