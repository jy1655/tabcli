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
    pub(in crate::native) cleanup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) cleanup_detail: Option<String>,
}
