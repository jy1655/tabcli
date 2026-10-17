use super::hook::record_correlated_failure;
use super::read_pending_turn;
use super::usage::reported_usage;
use crate::native::session::{Reader, Store, turn};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub(super) const HOOK_FAILURE_FILE: &str = "pi-hook-failure.json";

#[derive(Debug, Deserialize, Serialize)]
struct HookFailureSignal {
    error: String,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    #[serde(default)]
    claim_token: Option<String>,
    /// What the extension's payload reported, kept when delivery failed; the same shape and
    /// validation as the hook payload (`usage` or a bounded `usage_error`). Both are kept raw
    /// so that a malformed field never rejects the signal before its claim is checked:
    /// [`reported_usage`] bounds them after the claim matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage_error: Option<serde_json::Value>,
}

pub(in crate::native::provider) struct PiFailureMonitor {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<()>>>,
}

impl PiFailureMonitor {
    pub(in crate::native::provider) fn start(directory: &Path) -> Result<Self> {
        let directory = directory.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let error_directory = directory.clone();
        let handle = thread::Builder::new()
            .name("agent-bridge-pi-failure-monitor".to_owned())
            .spawn(move || {
                let result = monitor_hook_failures(&directory, &stop_for_thread);
                if let Err(error) = &result {
                    let _ = turn::Report::monitor_failure(
                        &Store::open_unchecked(&error_directory),
                        FirstPartyCli::Pi,
                        &format!("Pi result recovery monitor failed: {error:#}"),
                    );
                }
                result
            })
            .context("failed to start Pi result recovery monitor")?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    pub(in crate::native::provider) fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .take()
            .context("Pi result recovery monitor handle was already consumed")?
            .join()
            .map_err(|_| anyhow::anyhow!("Pi result recovery monitor panicked"))??;
        Ok(())
    }
}

impl Drop for PiFailureMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn monitor_hook_failures(directory: &Path, stop: &AtomicBool) -> Result<()> {
    loop {
        consume_hook_failure(directory)?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
pub(super) fn consume_hook_failure_for_test(directory: &Path) -> Result<bool> {
    consume_hook_failure(directory)
}

fn consume_hook_failure(directory: &Path) -> Result<bool> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(HOOK_FAILURE_FILE)
        .text()?
    else {
        return Ok(false);
    };
    let signal: HookFailureSignal =
        serde_json::from_str(&text).context("invalid Pi hook failure recovery signal")?;
    let error = signal.error.trim();
    if error.is_empty() {
        bail!("Pi hook failure recovery signal has no error");
    }
    Store::open_unchecked(directory)
        .private(HOOK_FAILURE_FILE)
        .remove()
        .context("failed to consume Pi hook failure recovery signal")?;
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(true);
    };
    if signal.claim_token.as_deref() != Some(pending.claim_token.as_str()) {
        return Ok(true);
    }
    let mut reported = serde_json::Map::new();
    if let Some(usage) = signal.usage {
        reported.insert("usage".to_owned(), usage);
    }
    if let Some(error) = signal.usage_error {
        reported.insert("usage_error".to_owned(), error);
    }
    let usage = reported_usage(&serde_json::Value::Object(reported));
    record_correlated_failure(
        directory,
        error,
        signal.provider_session_id,
        signal.turn_id,
        &pending,
        usage,
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests;
