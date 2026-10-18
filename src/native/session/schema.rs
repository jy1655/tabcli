//! Shared durable session record shapes and their schema limits.
use crate::native::terminal;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(in crate::native) const SESSION_SCHEMA: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
pub(in crate::native) struct SessionManifest {
    pub(in crate::native) schema: u32,
    pub(in crate::native) id: String,
    pub(in crate::native) provider: String,
    pub(in crate::native) provider_path: PathBuf,
    pub(in crate::native) provider_version: String,
    pub(in crate::native) workspace: PathBuf,
    pub(in crate::native) title: String,
    #[serde(default)]
    pub(in crate::native) model: Option<String>,
    #[serde(default)]
    pub(in crate::native) effort: Option<String>,
    pub(in crate::native) yolo: bool,
    pub(in crate::native) created_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(in crate::native) struct SessionEvent {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(in crate::native) cancelled: bool,
    pub(in crate::native) provider: String,
    pub(in crate::native) message: String,
    #[serde(default)]
    pub(in crate::native) error: Option<String>,
    pub(in crate::native) provider_session_id: Option<String>,
    pub(in crate::native) turn_id: Option<String>,
    #[serde(default)]
    pub(in crate::native) created_unix_ms: Option<u128>,
    /// Adapter-selected completion signal, also retained in the journal event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) provenance: Option<String>,
    /// Usage the provider itself reported for this request, when its signal carries one.
    /// Never part of the completion outcome; see [`ProviderUsage`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) usage: Option<ProviderUsage>,
    /// Why no `usage` was recorded although the provider's signal carried one: a bounded
    /// phrase (`USAGE_ERROR_LIMIT`) naming the malformed field or the size policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) usage_error: Option<String>,
}

/// Longest `usage_error` a record may carry; writers truncate before recording.
pub(in crate::native) const USAGE_ERROR_LIMIT: usize = 200;

/// The `usage_error` of a completion whose usage was dropped by the event size policy.
pub(in crate::native) const USAGE_OMITTED_SIZE: &str = "omitted: event size";

/// Tokens a provider reported for one request, taken only from a signal the provider
/// documents as usage. Provider-neutral shape; `source` names the documented signal
/// (today only `pi_extension_agent_end`), `scope` is always `request`, and `coverage` is
/// `complete` when every run of the request ended with an ordinary stop and `uncertain`
/// otherwise. No cost, no estimate, no total across requests.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(in crate::native) struct ProviderUsage {
    pub(in crate::native) source: String,
    pub(in crate::native) scope: String,
    pub(in crate::native) coverage: String,
    pub(in crate::native) runs: u32,
    pub(in crate::native) assistant_messages: u32,
    pub(in crate::native) input_tokens: u64,
    pub(in crate::native) output_tokens: u64,
    pub(in crate::native) cache_read_tokens: u64,
    pub(in crate::native) cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) reasoning_tokens: Option<u64>,
    pub(in crate::native) total_tokens: u64,
}

/// Truncates a usage error to [`USAGE_ERROR_LIMIT`] characters, so that it can never be the
/// oversized part of an event.
pub(in crate::native) fn bounded_usage_error(error: &str) -> String {
    error.chars().take(USAGE_ERROR_LIMIT).collect()
}

// The provider process a launch wrapper spawned for its session (`PROVIDER_PROCESS_FILE`).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(in crate::native) struct ProviderProcessRecord {
    pub(in crate::native) schema: u32,
    pub(in crate::native) managed_session_id: String,
    pub(in crate::native) pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) windows_process_identity: Option<terminal::WindowsProcessIdentity>,
    pub(in crate::native) spawned_unix_ms: u128,
}

/// Largest event the publication predicate compares with its journal. It bounds only that
/// comparison: a larger journaled event is never read for a verdict and never published,
/// and the size policy at journal creation keeps new completions under it. It is the byte
/// budget of a whole `search`, so a search can afford at most one such comparison. It
/// does not bound the read of an ordinary, non-journaled event by `result`, `inspect`, or
/// `--context-result`; only a search bounds those reads, with its byte budget.
pub(in crate::native) const EVENT_READ_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
pub(in crate::native) struct RecordedReopenRefusal {
    pub(in crate::native) schema: u32,
    pub(in crate::native) phase: String,
    pub(in crate::native) gate: String,
    pub(in crate::native) detail: String,
    pub(in crate::native) created_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) claim_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) cleanup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) cleanup_detail: Option<String>,
}
