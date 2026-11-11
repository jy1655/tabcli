//! The completion module's own records: their names and schemas, the validity each one must
//! meet, the answers callers read from them, and the ways they are read.
//!
//! Launch records, one file each:
//! - `claude-completion-mode.json` and `claude-question-mode.json` are this launch's decisions
//!   (**Launch decision** in CONTEXT.md): `prepare` records them once, and every later command
//!   reads them, never the current version or setting.
//! - `claude-mod-ready.json` is the module's readiness evidence, replaced at each
//!   `session.start`; `claude-mod-diagnostic.json` is the module signal rejected last.
//!
//! Per-Claim records, one file per Claim token, written under the lifecycle lock. Each names
//! its file from its own Claim:
//! - `claude-mod-bound-<token>.json` ([`Binding`]): the exact-frame `turn.start` of the
//!   pending request, written once and never replaced.
//! - `claude-mod-candidate-<token>.json` ([`Candidate`]): the first unmarked bound answer.
//! - `claude-mod-continuation-<token>.json` ([`Continuation`]): the selected marked
//!   continuation, kept so that an interrupted publication cannot select another turn.
//! - `claude-questions-<token>.json` ([`QuestionRecord`]): the question emissions retained
//!   from the bound turn.
//!
//! Each per-Claim record has one floor here; a caller adds its own condition on top of it
//! (completion requires the pending request's correlation id of a binding, selection the
//! incoming turn of a continuation, inspection a valid identity other than the bound turn).
//!
//! The read policies differ on purpose, and the completion tests pin the difference:
//! - [`read_live`], and the per-Claim `read` of a binding, candidate or continuation: delivery,
//!   hooks, completion and capability observations read the whole file and decode it after
//!   lossy UTF-8 conversion, an absent file being no record. The completion-mode record carries
//!   the whole `claude --version` output, which has no limit of its own.
//! - [`Binding::require`]: inspection requires the binding of a retained candidate and reads
//!   the whole file as strict UTF-8.
//! - [`QuestionRecord::read`]: at most [`QUESTION_RECORD_BYTES`] of strict UTF-8, held to the
//!   record's floor for the Receipt it is read for.
//! - [`read_doctor`]: doctor reads at most [`DOCTOR_READ_LIMIT`] bytes of strict UTF-8 and
//!   classifies what does not decode. A record past that bound tells doctor nothing; admission
//!   reads it again with the live policy.
use super::{valid_claude_conversation_id, valid_identity};
use crate::native::provider::QuestionEmission;
use crate::native::session::{Reader, Store, primitives::unix_ms, requests::Receipt};
use anyhow::{Context, Result, bail};
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

// A name takes the token as given: the caller supplies the current Claim's or a Receipt's.
pub(super) fn binding_name(token: &str) -> String {
    format!("claude-mod-bound-{token}.json")
}

pub(super) fn candidate_name(token: &str) -> String {
    format!("claude-mod-candidate-{token}.json")
}

pub(super) fn continuation_name(token: &str) -> String {
    format!("claude-mod-continuation-{token}.json")
}

pub(super) fn questions_name(token: &str) -> String {
    format!("claude-questions-{token}.json")
}

/// The immutable binding of a Claim to the official turn whose `turn.start` carried the pending
/// request's exact frame.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(super) struct Binding {
    pub(super) schema: u32,
    pub(super) claim: String,
    pub(super) correlation_id: String,
    pub(super) conversation: String,
    pub(super) turn_id: String,
}

impl Binding {
    pub(super) fn new(
        token: &str,
        correlation_id: &str,
        conversation: &str,
        turn_id: &str,
    ) -> Self {
        Self {
            schema: 1,
            claim: token.into(),
            correlation_id: correlation_id.into(),
            conversation: conversation.into(),
            turn_id: turn_id.into(),
        }
    }

    /// The floor of every binding read for the Claim `token`: this schema, that Claim, a Claude
    /// conversation and a turn identity. The correlation id is the caller's condition.
    pub(super) fn valid_for(&self, token: &str) -> bool {
        self.schema == 1
            && self.claim == token
            && valid_claude_conversation_id(&self.conversation)
            && valid_identity(&self.turn_id)
    }

    /// The live read.
    pub(super) fn read(reader: &Reader, token: &str) -> Result<Option<Self>> {
        reader.private(&binding_name(token)).optional_json()
    }

    /// Inspection's read: a retained candidate requires its binding, read whole as strict UTF-8.
    pub(super) fn require(reader: &Reader, token: &str) -> Result<Self> {
        reader.private(&binding_name(token)).json()
    }

    pub(super) fn write(&self, store: &Store) -> Result<()> {
        store.private(&binding_name(&self.claim)).write_json(self)
    }
}

/// The first unmarked answer of the bound turn: not a result, and never replaced.
#[derive(Deserialize, Serialize)]
pub(super) struct Candidate {
    pub(super) schema: u32,
    pub(super) binding: Binding,
    pub(super) answer_preview: String,
    pub(super) created_unix_ms: u128,
}

impl Candidate {
    /// A candidate observed now; `answer_preview` is already bounded by the caller.
    pub(super) fn new(binding: &Binding, answer_preview: String) -> Self {
        Self {
            schema: 1,
            binding: binding.clone(),
            answer_preview,
            created_unix_ms: unix_ms(),
        }
    }

    /// The floor every reader of a candidate requires: this schema, the original binding, a
    /// creation time and a preview of at most 1024 characters.
    pub(super) fn matches(&self, binding: &Binding) -> bool {
        self.schema == 1
            && self.binding == *binding
            && self.created_unix_ms > 0
            && self.answer_preview.chars().count() <= 1024
    }

    /// The live read.
    pub(super) fn read(reader: &Reader, token: &str) -> Result<Option<Self>> {
        reader.private(&candidate_name(token)).optional_json()
    }

    pub(super) fn write(&self, store: &Store) -> Result<()> {
        store
            .private(&candidate_name(&self.binding.claim))
            .write_json(self)
    }
}

/// The marked continuation selected to settle the Claim, with both official turn identities.
#[derive(Deserialize, Serialize)]
pub(super) struct Continuation {
    pub(super) schema: u32,
    pub(super) binding: Binding,
    pub(super) turn_id: String,
    pub(super) created_unix_ms: u128,
}

impl Continuation {
    /// The continuation `turn_id` of `binding`, selected now.
    pub(super) fn new(binding: &Binding, turn_id: &str) -> Self {
        Self {
            schema: 1,
            binding: binding.clone(),
            turn_id: turn_id.into(),
            created_unix_ms: unix_ms(),
        }
    }

    /// The floor every reader of a selection requires: this schema, the original binding and a
    /// creation time. Which turn it may name is the caller's condition.
    pub(super) fn matches(&self, binding: &Binding) -> bool {
        self.schema == 1 && self.binding == *binding && self.created_unix_ms != 0
    }

    /// The live read.
    pub(super) fn read(reader: &Reader, token: &str) -> Result<Option<Self>> {
        reader.private(&continuation_name(token)).optional_json()
    }

    pub(super) fn write(&self, store: &Store) -> Result<()> {
        store
            .private(&continuation_name(&self.binding.claim))
            .write_json(self)
    }
}

/// The whole pretty-encoded question record's bound, Request identity included.
pub(super) const QUESTION_RECORD_BYTES: u64 = 64 * 1024;
/// The largest step or tool index of a retained item's position.
pub(super) const MAX_INDEX: u64 = 9_007_199_254_740_991;

pub(super) use question_record::Record as QuestionRecord;

// The struct keeps the name `Record`: the decoder's refusals name it ("expected struct Record"),
// and their text reaches the hook's error.
mod question_record {
    use super::{Binding, QuestionEmission};
    use serde::{Deserialize, Serialize};

    /// The question emissions retained from the bound turn of a Request's Claim: a prefix in
    /// position order, and whether the observation is incomplete or the record full.
    #[derive(Deserialize, Serialize)]
    pub(in crate::native::provider::claude::completion) struct Record {
        pub(in crate::native::provider::claude::completion) schema: u32,
        pub(in crate::native::provider::claude::completion) request_id: String,
        pub(in crate::native::provider::claude::completion) binding: Binding,
        pub(in crate::native::provider::claude::completion) items: Vec<QuestionEmission>,
        pub(in crate::native::provider::claude::completion) incomplete: bool,
        pub(in crate::native::provider::claude::completion) full: bool,
    }
}

impl QuestionRecord {
    /// The record of `request_id` before any emission.
    pub(super) fn empty(request_id: String, binding: &Binding) -> Self {
        Self {
            schema: 1,
            request_id,
            binding: binding.clone(),
            items: vec![],
            incomplete: false,
            full: false,
        }
    }

    /// The floor of a record read for `receipt`: this schema and Request, a binding of the
    /// Receipt's Claim with a valid correlation identity, `full` only when incomplete, the
    /// record bound, and a prefix of valid items in strictly increasing position.
    pub(super) fn check(&self, receipt: &Receipt) -> Result<()> {
        if self.schema != 1
            || self.request_id != receipt.request_id
            || !self.binding.valid_for(&receipt.claim_token)
            || !valid_identity(&self.binding.correlation_id)
            || (self.full && !self.incomplete)
            || !self.fits()
        {
            bail!("invalid Claude question record");
        }
        let mut previous = None;
        for item in &self.items {
            let current = position(&item.id).context("invalid Claude question position")?;
            if previous.is_some_and(|previous| previous >= current)
                || !(1..=4).contains(&item.questions.len())
                || item.questions.iter().any(|q| {
                    q.title.trim().is_empty()
                        || q.options.as_ref().is_none_or(|options| {
                            !(2..=4).contains(&options.len())
                                || options.iter().any(|label| label.trim().is_empty())
                        })
                })
            {
                bail!("invalid Claude retained question prefix");
            }
            previous = Some(current);
        }
        Ok(())
    }

    /// Whether the pretty encoding stays within [`QUESTION_RECORD_BYTES`].
    pub(super) fn fits(&self) -> bool {
        struct Limit(u64);
        impl std::io::Write for Limit {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_sub(bytes.len() as u64)
                    .ok_or_else(|| std::io::Error::other("Claude question record limit"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer_pretty(Limit(QUESTION_RECORD_BYTES), self).is_ok()
    }

    /// The record of the Receipt's Claim, read within its bound and held to its floor.
    pub(super) fn read(reader: &Reader, receipt: &Receipt) -> Result<Option<Self>> {
        let Some(bytes) = reader
            .private(&questions_name(&receipt.claim_token))
            .timeline(QUESTION_RECORD_BYTES)?
        else {
            return Ok(None);
        };
        let record: Self = serde_json::from_slice(&bytes)?;
        record.check(receipt)?;
        Ok(Some(record))
    }

    pub(super) fn write(&self, store: &Store) -> Result<()> {
        store
            .private(&questions_name(&self.binding.claim))
            .write_json(self)
    }
}

/// The step and tool index of a retained item's id, `step:<step>/tool:<index>` in canonical form.
pub(super) fn position(id: &str) -> Option<(u64, u64)> {
    let (step, tool) = id.strip_prefix("step:")?.split_once("/tool:")?;
    let step: u64 = step.parse().ok()?;
    let tool: u64 = tool.parse().ok()?;
    (step <= MAX_INDEX && tool <= MAX_INDEX && id == format!("step:{step}/tool:{tool}"))
        .then_some((step, tool))
}
