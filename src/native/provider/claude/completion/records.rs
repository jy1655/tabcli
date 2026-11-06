//! The completion module's own records: their names and schemas, the validity each one must
//! meet, the answers callers read from them, and the two ways they are read.
//!
//! - `claude-completion-mode.json` and `claude-question-mode.json` are this launch's decisions
//!   (**Launch decision** in CONTEXT.md): `prepare` records them once, and every later command
//!   reads them, never the current version or setting.
//! - `claude-mod-ready.json` is the module's readiness evidence, replaced at each
//!   `session.start`; `claude-mod-diagnostic.json` is the module signal rejected last.
//!
//! The two read policies differ on purpose, and the completion tests pin the difference:
//! - [`read_live`]: delivery, hooks and capability observations read the whole file and decode
//!   it after lossy UTF-8 conversion. The completion-mode record carries the whole
//!   `claude --version` output, which has no limit of its own.
//! - [`read_doctor`]: doctor reads at most [`DOCTOR_READ_LIMIT`] bytes of strict UTF-8 and
//!   classifies what does not decode. A record past that bound tells doctor nothing; admission
//!   reads it again with the live policy.
use super::valid_claude_conversation_id;
use crate::native::session::{Reader, Store};
use anyhow::{Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(super) const MODE_FILE: &str = "claude-completion-mode.json";
pub(super) const READY_FILE: &str = "claude-mod-ready.json";
pub(super) const DIAGNOSTIC_FILE: &str = "claude-mod-diagnostic.json";
/// The question deflection decided at launch; later reads use it, never the current setting.
pub(super) const QUESTION_MODE_FILE: &str = "claude-question-mode.json";
/// The module protocol revision a readiness record must carry.
pub(super) const REVISION: u32 = 1;
/// Doctor's bound: the records hold a few short fields; a diagnostic holds two 1024-character
/// texts.
pub(super) const DOCTOR_READ_LIMIT: u64 = 16 * 1024;

/// A record of this module, at its own file in the session directory.
pub(super) trait Record: Serialize + DeserializeOwned {
    const FILE: &'static str;
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Mode {
    NativeV1,
    LegacyStop,
}

/// The completion protocol this launch selected. No record is a session launched before the
/// record existed, which gets legacy treatment and is never upgraded from a version.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub(super) struct LaunchMode {
    pub(super) schema: u32,
    pub(super) mode: Mode,
    pub(super) reason: String,
    pub(super) claude_version: Option<String>,
}

impl Record for LaunchMode {
    const FILE: &'static str = MODE_FILE;
}

impl LaunchMode {
    /// The record a native-mode launch writes; `version` is the recorded version output.
    pub(super) fn native(windows: bool, version: String) -> Self {
        Self {
            schema: 1,
            mode: Mode::NativeV1,
            reason: if windows {
                "windows_stable_2_1_302_or_newer"
            } else {
                "macos_stable_2_1_302_or_newer"
            }
            .into(),
            claude_version: Some(version),
        }
    }

    /// Schema 1 is the only schema this binary reads.
    pub(super) fn supported(&self) -> bool {
        self.schema == 1
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Deflection {
    On,
    Off,
}

/// Whether the question plugin of this launch refuses a model's main-loop `AskUserQuestion`
/// call and returns the question in the Result. It records configuration, not execution.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub(super) struct QuestionMode {
    pub(super) schema: u32,
    pub(super) deflection: Deflection,
}

impl Record for QuestionMode {
    const FILE: &'static str = QUESTION_MODE_FILE;
}

impl QuestionMode {
    pub(super) fn new(deflection: Deflection) -> Self {
        Self {
            schema: 1,
            deflection,
        }
    }

    /// Schema 1 is the only schema this binary reads.
    pub(super) fn supported(&self) -> bool {
        self.schema == 1
    }
}

/// The conversation of the latest `session.start` the module reported.
#[derive(Deserialize, Serialize)]
pub(super) struct Ready {
    pub(super) schema: u32,
    pub(super) revision: u32,
    pub(super) conversation: String,
}

impl Record for Ready {
    const FILE: &'static str = READY_FILE;
}

impl Ready {
    pub(super) fn new(conversation: &str) -> Self {
        Self {
            schema: 1,
            revision: REVISION,
            conversation: conversation.into(),
        }
    }

    /// The schema and module revision this binary accepts.
    pub(super) fn recognized(&self) -> bool {
        self.schema == 1 && self.revision == REVISION
    }

    /// A recognized record that names a Claude conversation: what admission accepts.
    pub(super) fn valid(&self) -> bool {
        self.recognized() && valid_claude_conversation_id(&self.conversation)
    }
}

// The latest rejected module signal. It names no request and no time.
#[derive(Deserialize, Serialize)]
pub(super) struct Diagnostic {
    pub(super) schema: u32,
    pub(super) event: String,
    pub(super) reason: String,
}

impl Record for Diagnostic {
    const FILE: &'static str = DIAGNOSTIC_FILE;
}

impl Diagnostic {
    /// Schema 1 is the only schema this binary reads.
    pub(super) fn supported(&self) -> bool {
        self.schema == 1
    }
}

/// The live read: the whole file, decoded after lossy UTF-8 conversion. A file that is not
/// regular, or that does not decode, is an error naming the file.
pub(super) fn read_live<T: Record>(reader: &Reader) -> Result<Option<T>> {
    reader.private(T::FILE).optional_json()
}

/// Doctor's read: at most [`DOCTOR_READ_LIMIT`] bytes of strict UTF-8, left undecoded so that
/// doctor can classify what does not decode.
pub(super) fn read_doctor<T: Record>(reader: &Reader) -> Result<Option<Vec<u8>>> {
    reader.private(T::FILE).timeline(DOCTOR_READ_LIMIT)
}

pub(super) fn write<T: Record>(store: &Store, record: &T) -> Result<()> {
    store.private(T::FILE).write_json(record)
}

/// Whether this launch selected native completion: no record means it did not, and an
/// unreadable or unsupported record is an error.
pub(in crate::native::provider::claude) fn native_mode(directory: &Path) -> Result<bool> {
    match read_live::<LaunchMode>(&Reader::open_unchecked(directory))? {
        Some(mode) if mode.supported() => Ok(mode.mode == Mode::NativeV1),
        Some(_) => bail!("unsupported Claude completion mode schema"),
        None => Ok(false),
    }
}

/// The question deflection this launch recorded: none for a launch before the record existed,
/// and an error for an unreadable or unsupported record.
pub(super) fn question_deflection(directory: &Path) -> Result<Option<Deflection>> {
    match read_live::<QuestionMode>(&Reader::open_unchecked(directory))? {
        Some(mode) if mode.supported() => Ok(Some(mode.deflection)),
        Some(_) => bail!("unsupported Claude question mode schema"),
        None => Ok(None),
    }
}
