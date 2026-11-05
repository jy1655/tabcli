//! The Codex adapter's launch decisions: what a launch decided and configured for the injected
//! hooks, recorded before the hooks value is returned. Two records, both written by
//! `hooks::prepare` and read only here:
//! - the lifecycle record says whether the `UserPromptSubmit` and `Stop` hooks were injected;
//! - the notice record says whether the question notice hook was injected.
//!
//! Every later command reads a record, never the current settings or a new probe. A record is
//! configuration, not evidence that Codex trusted or ran what it describes. Every reader goes
//! through [`read`], so one bound, strict UTF-8 and one validity floor hold for the initial-paste
//! gate, the capability report, the doctor and the notice hook. Each caller decides what an
//! unreadable record means for it; a missing record is a session launched by an earlier binary,
//! which injected nothing that record describes.
use super::hooks::{NOTICE_VECTOR_VERSION, VECTOR_VERSION};
use crate::native::session::{Reader, Store};
use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(super) const FILE: &str = "codex-launch-mode.json";
/// The question notice hook's launch decision; every later read uses it, never the setting.
pub(super) const NOTICE_FILE: &str = "codex-question-notice.json";

/// Both records are a few hundred bytes, except that the lifecycle record carries the whole
/// output of `codex --version`, which has no limit of its own. The bound leaves room for a long
/// banner there; only a version output of nearly this size would leave that launch's own record
/// unreadable.
const READ_LIMIT: u64 = 64 * 1024;

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum LifecycleHooks {
    Injected,
    NotInjected,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reason {
    #[serde(rename = "version_below_0_163")]
    VersionBelow0_163,
    VersionUnknown,
    Prerelease,
    SettingsOff,
    LauncherUnavailable,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub(super) struct LaunchMode {
    pub(super) schema: u32,
    pub(super) lifecycle_hooks: LifecycleHooks,
    pub(super) reason: Option<Reason>,
    pub(super) codex_version: Option<String>,
    pub(super) vector_codex_version: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum NoticeHook {
    Injected,
    NotInjected,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum NoticeReason {
    SettingsOff,
    LifecycleHooksNotInjected,
    #[serde(rename = "version_below_0_166")]
    VersionBelow0_166,
    LauncherUnavailable,
}

/// Whether this launch's `-c hooks=` value carried the question notice hook. It records
/// configuration: neither that Codex trusted or ran the hook nor that a child read the notice.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub(super) struct NoticeMode {
    pub(super) schema: u32,
    pub(super) notice_hook: NoticeHook,
    pub(super) reason: Option<NoticeReason>,
    pub(super) vector_codex_version: String,
}

/// What one bounded read of a launch decision found.
#[derive(Debug)]
pub(super) enum Decision<T> {
    /// No record: the session was launched by a binary that did not write it, and injected
    /// nothing the record describes.
    Absent,
    /// A record Bridge cannot use: unreadable, over the bound, not UTF-8, malformed JSON, or one
    /// that fails the validity floor. Each caller decides what that means for it.
    Unreadable(anyhow::Error),
    /// A record that passed the floor.
    Present(T),
}

impl<T> Decision<T> {
    fn answer(self, question: impl FnOnce(&T) -> bool) -> Result<bool> {
        match self {
            Self::Absent => Ok(false),
            Self::Unreadable(error) => Err(error),
            Self::Present(record) => Ok(question(&record)),
        }
    }
}

impl Decision<LaunchMode> {
    /// Whether this launch injected the lifecycle hooks; no record means it did not, and an
    /// unreadable record is its error.
    pub(super) fn lifecycle_hooks_injected(self) -> Result<bool> {
        self.answer(LaunchMode::lifecycle_hooks_injected)
    }
}

impl Decision<NoticeMode> {
    /// Whether this launch injected the question notice hook; no record means it did not, and an
    /// unreadable record is its error.
    pub(super) fn notice_hook_injected(self) -> Result<bool> {
        self.answer(NoticeMode::notice_hook_injected)
    }
}

impl LaunchMode {
    pub(super) fn lifecycle_hooks_injected(&self) -> bool {
        self.lifecycle_hooks == LifecycleHooks::Injected
    }

    // Schema 1, the pinned hash vector, and a reason exactly when the hooks were left out.
    fn valid(&self) -> bool {
        self.schema == 1
            && self.vector_codex_version == VECTOR_VERSION
            && self.lifecycle_hooks_injected() == self.reason.is_none()
    }
}

impl NoticeMode {
    pub(super) fn notice_hook_injected(&self) -> bool {
        self.notice_hook == NoticeHook::Injected
    }

    // Schema 1, the pinned hash vector, and a reason exactly when the hook was left out.
    fn valid(&self) -> bool {
        self.schema == 1
            && self.vector_codex_version == NOTICE_VECTOR_VERSION
            && self.notice_hook_injected() == self.reason.is_none()
    }
}

/// Records the lifecycle hooks decision: `reason` is why they were left out, `None` that they
/// were injected. `codex_version` is the version output the launch recorded.
pub(super) fn record_lifecycle(
    store: &Store,
    reason: Option<Reason>,
    codex_version: Option<String>,
) -> Result<()> {
    store.private(FILE).write_json(&LaunchMode {
        schema: 1,
        lifecycle_hooks: if reason.is_none() {
            LifecycleHooks::Injected
        } else {
            LifecycleHooks::NotInjected
        },
        reason,
        codex_version,
        vector_codex_version: VECTOR_VERSION.into(),
    })
}

/// Records the question notice hook decision: `reason` is why it was left out, `None` that it
/// was injected.
pub(super) fn record_notice(store: &Store, reason: Option<NoticeReason>) -> Result<()> {
    store.private(NOTICE_FILE).write_json(&NoticeMode {
        schema: 1,
        notice_hook: if reason.is_none() {
            NoticeHook::Injected
        } else {
            NoticeHook::NotInjected
        },
        reason,
        vector_codex_version: NOTICE_VECTOR_VERSION.into(),
    })
}

/// The lifecycle hooks decision of the launch that created `directory`.
pub(super) fn lifecycle(directory: &Path) -> Decision<LaunchMode> {
    read(
        directory,
        FILE,
        "failed to parse Codex launch mode",
        "Agent Bridge rejected invalid Codex launch mode",
        LaunchMode::valid,
    )
}

/// The question notice hook decision of the launch that created `directory`.
pub(super) fn notice(directory: &Path) -> Decision<NoticeMode> {
    read(
        directory,
        NOTICE_FILE,
        "failed to parse Codex question notice mode",
        "Agent Bridge rejected invalid Codex question notice mode",
        NoticeMode::valid,
    )
}

/// Reads a record within its bound as strict UTF-8 JSON and applies its validity floor once.
fn read<T: DeserializeOwned>(
    directory: &Path,
    file: &str,
    malformed: &'static str,
    invalid: &'static str,
    valid: impl FnOnce(&T) -> bool,
) -> Decision<T> {
    let bytes = match Reader::open_unchecked(directory)
        .private(file)
        .timeline(READ_LIMIT)
    {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Decision::Absent,
        Err(error) => return Decision::Unreadable(error),
    };
    let record: T = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(error) => return Decision::Unreadable(anyhow::Error::new(error).context(malformed)),
    };
    if valid(&record) {
        Decision::Present(record)
    } else {
        Decision::Unreadable(anyhow::Error::msg(invalid))
    }
}

#[cfg(test)]
impl<T: std::fmt::Debug> Decision<T> {
    /// The record, for a test that expects one.
    pub(super) fn unwrap(self) -> T {
        match self {
            Self::Present(record) => record,
            other => panic!("expected a present Codex launch decision, found {other:?}"),
        }
    }

    pub(super) fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub(super) fn is_unreadable(&self) -> bool {
        matches!(self, Self::Unreadable(_))
    }
}

#[cfg(test)]
mod tests;
