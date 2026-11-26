//! Whether a Session belongs to another OS, judged from its own Records, for the three steps
//! that change a Session's Records: the explicit close, prune and Repair.
//!
//! A state directory can be copied or moved between machines. A Session belongs to another OS
//! only on a positive statement in a Record that is this Session's own:
//! - its active handle, bound to this Session, states a kind this OS does not run, or, for a
//!   Herdr handle only, records a host birth of another platform's variant;
//! - its consumption marker says `consumed: true` and names a kind this OS does not run;
//! - its Owner record, bound to this Session, carries the identity another OS's writer records
//!   (macOS: the process start time; Linux: the Linux birth; Windows: the Windows process
//!   identity).
//!
//! Nothing else is evidence: the kind the reader supplies for a handle that omits it (iTerm2,
//! the legacy default), a field that does not belong to the handle's kind, a Record bound to
//! another Session or to none, a claimed handle, a marker that does not decode, repeats a
//! field or does not say `consumed: true`, and anything missing, null or refused by its
//! reader. Without evidence the Session is this OS's, and the step is main's.
//!
//! The judgment (`judge`) is a pure function of Records the step decoded once, with the
//! readers it uses for its own work: the Owner record's one observation, which the step then
//! attests or not, and the handle Records (`HandleRecords`) that a settlement then acts on.
//! When Records name different OSes, the first statement in this order opens the refusal: the
//! active handle, the consumption marker, the Owner record.
//!
//! Each of the three steps takes one early branch on that judgment, complete in itself:
//! Repair and launch repair leave such a Session as it is instead of attesting its owner here,
//! prune does not count its owner, and the close transaction settles it from its Records when
//! they show it ended (`ended`) and refuses any other. Every other command answers for such a
//! Session as for any Session.
use super::ownership::{NativeSessionOwner, OwnerBinding};
use super::{OsTerminals, TerminalKind, TerminalSession};
use crate::native::failure_kind::Unsupported;
use crate::native::session::owner::RecordedOwner;
use crate::native::session::record::{Consumption, StatedTerminal};
use crate::native::session::{self, CoreRecord, Reader};
use std::path::Path;

/// What a Session's own Records name as another OS, with the refusal it gives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum Elsewhere {
    /// The bound active handle, or the consumption marker, names a kind this OS does not run.
    Kind(TerminalKind),
    /// A bound Herdr handle's host birth or the bound Owner record's identity names another
    /// OS (or several).
    Recorded(&'static str),
}

impl Elsewhere {
    /// What the Records name, the same on every OS: the kind's refusal when a Record names a
    /// kind this OS does not run (`<Name> is not available on <OS>`), otherwise the OS the
    /// Records name. It names no terminal to use instead: only a new launch could choose one.
    pub(in crate::native) fn head(self) -> String {
        let here = OsTerminals::CURRENT.map_or("this operating system", |os| os.os);
        match self {
            Self::Kind(kind) => format!("{} is not available on {here}", kind.display_name()),
            Self::Recorded(origin) => {
                format!("a session recorded on {origin} is not available on {here}")
            }
        }
    }

    /// The explicit close's refusal of such a session whose Records leave it running, with
    /// the directory of its Records. It says what is true: nothing here can judge or end the
    /// session, and an explicit close settles it once its Records show it ended.
    pub(in crate::native) fn refusal(self, directory: &Path) -> anyhow::Error {
        anyhow::Error::new(Unsupported::new(format!(
            "{}: TAB on this machine cannot judge or end this session; once its records show \
             it ended, an explicit close settles it (its records are in {})",
            self.head(),
            directory.display()
        )))
    }

    /// How the close transaction closed such a session: from its Records alone.
    pub(in crate::native) fn settled(self) -> String {
        format!(
            "settled from its records alone ({}); no terminal or process was observed or closed",
            self.head()
        )
    }
}

/// Whether a session of another OS's Records show it ended, so that the explicit close settles
/// it from them alone: it exited or failed. A session in any other open state is not ended
/// from here; a closed one already is.
pub(in crate::native) fn ended(state: &session::SessionState) -> bool {
    matches!(
        state,
        session::SessionState::Exited | session::SessionState::Failed
    )
}

/// The OSes whose Owner writer records the identity fields this record carries.
fn recorded_on(owner: &NativeSessionOwner) -> Vec<OsTerminals> {
    let mut systems = Vec::new();
    if owner.process_start_seconds.is_some() || owner.process_start_microseconds.is_some() {
        systems.push(OsTerminals::MACOS);
    }
    if owner.linux_birth.is_some() {
        systems.push(OsTerminals::LINUX);
    }
    if owner.windows_process_identity.is_some() {
        systems.push(OsTerminals::WINDOWS);
    }
    systems
}

/// The OS whose Herdr host a Herdr handle's recorded birth describes.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn herdr_birth_os(handle: &TerminalSession) -> Option<OsTerminals> {
    use super::record::HerdrHostBirth;
    Some(match handle.herdr_core()?.birth {
        HerdrHostBirth::Macos { .. } => OsTerminals::MACOS,
        HerdrHostBirth::Linux { .. } => OsTerminals::LINUX,
        HerdrHostBirth::Windows { .. } => OsTerminals::WINDOWS,
    })
}

// A platform without a managed terminal records no Herdr host.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn herdr_birth_os(_: &TerminalSession) -> Option<OsTerminals> {
    None
}

/// What a handle itself says on `os`: a kind `os` does not run, or, for a Herdr handle only,
/// a recorded host birth of another platform's variant. It says nothing about whose handle it
/// is or whether its kind was stated.
fn of_handle_on(os: OsTerminals, handle: &TerminalSession) -> Option<Elsewhere> {
    if !os.runs(handle.kind) {
        return Some(Elsewhere::Kind(handle.kind));
    }
    if handle.kind != TerminalKind::Herdr {
        return None;
    }
    let born = herdr_birth_os(handle)?;
    (born.os != os.os).then_some(Elsewhere::Recorded(born.os))
}

/// What an Owner record's identity says on `os`: an identity `os` does not record.
fn of_owner_on(os: OsTerminals, owner: &NativeSessionOwner) -> Option<Elsewhere> {
    let recorded = recorded_on(owner);
    if !recorded.iter().any(|system| system.os != os.os) {
        return None;
    }
    Some(Elsewhere::Recorded(match recorded.as_slice() {
        [one] => one.os,
        _ => "more than one operating system",
    }))
}

/// Whether a handle is this session's own for evidence: it names the session, with a
/// surface id. A legacy handle that names no session is no evidence.
fn bound_here(handle: &TerminalSession, session: &str) -> bool {
    handle.managed_session_id.as_deref() == Some(session) && !handle.id.is_empty()
}

/// The Records the judgment reads, as one step decoded them, once. A Record its reader
/// refused is `None` here.
#[derive(Clone, Copy, Default)]
pub(in crate::native) struct Records<'a> {
    /// The active handle (never a claimed one), with whether its bytes stated its kind.
    pub(in crate::native) active: Option<&'a StatedTerminal>,
    /// The consumption marker, when it decodes strictly and says `consumed: true`.
    pub(in crate::native) marker: Option<Consumption>,
    /// The Owner record as the step's one observation of it decoded it.
    pub(in crate::native) owner: Option<&'a RecordedOwner>,
}

/// Whether the Records of `session` positively say it belongs to another OS, and which.
pub(in crate::native) fn judge(session: &str, records: Records<'_>) -> Option<Elsewhere> {
    judge_on(OsTerminals::CURRENT?, session, records)
}

fn judge_on(os: OsTerminals, session: &str, records: Records<'_>) -> Option<Elsewhere> {
    if let Some(active) = records.active
        && active.kind_stated
        && bound_here(&active.handle, session)
        && let Some(elsewhere) = of_handle_on(os, &active.handle)
    {
        return Some(elsewhere);
    }
    if let Some(kind) = records.marker.and_then(|marker| marker.kind)
        && !os.runs(kind)
    {
        return Some(Elsewhere::Kind(kind));
    }
    let owner = records.owner?;
    (owner.binding == OwnerBinding::Bound)
        .then(|| of_owner_on(os, &owner.owner))
        .flatten()
}

/// A Session's handle Records as one step reads them, once, with the readers the step uses
/// for its own work: the active handle through the handle reader (`record::terminal_stated`,
/// which is `record::terminal`'s decode keeping whether the kind was stated), and the
/// consumption marker's bytes, which `record::consumption` decodes strictly. A settlement acts
/// on exactly these.
pub(in crate::native) struct HandleRecords {
    /// The active handle: its read and decode, or the reader's refusal.
    pub(in crate::native) active: anyhow::Result<Option<StatedTerminal>>,
    /// The consumption marker's bytes; `None` when it is absent or its reader refused it.
    pub(in crate::native) marker: Option<Vec<u8>>,
}

impl HandleRecords {
    pub(in crate::native) fn read(reader: &Reader) -> Self {
        Self {
            active: reader
                .record(CoreRecord::Terminal)
                .optional_json_with(session::record::terminal_stated),
            marker: reader
                .record(CoreRecord::TerminalClosed)
                .bytes()
                .ok()
                .flatten(),
        }
    }

    /// The completed consumption the marker states, if it states one strictly.
    pub(in crate::native) fn consumption(&self) -> Option<Consumption> {
        session::record::consumption(self.marker.as_deref()?)
    }

    /// What these Records and the step's decoded Owner record say about `session`.
    pub(in crate::native) fn elsewhere(
        &self,
        session: &str,
        owner: Option<&RecordedOwner>,
    ) -> Option<Elsewhere> {
        judge(
            session,
            Records {
                active: self.active.as_ref().ok().and_then(Option::as_ref),
                marker: self.consumption(),
                owner,
            },
        )
    }
}

/// The session id a reader's directory names.
pub(in crate::native) fn session_id(reader: &Reader) -> &str {
    reader
        .directory()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
}

/// The binding a settlement requires of every handle it removes: the handle names this
/// session (a legacy iTerm2 handle that names none counts as this session's) and has a
/// surface id.
pub(in crate::native) fn binding(handle: &TerminalSession, session: &str) -> anyhow::Result<()> {
    handle.verify_managed_session(session)?;
    if handle.id.is_empty() {
        anyhow::bail!("the recorded terminal handle is not bound to session {session}");
    }
    Ok(())
}

/// The judgment of the Session `reader` opens as the close transaction makes it: its handle
/// Records and its Owner record, each read once.
#[cfg(test)]
pub(in crate::native) fn of_records(reader: &Reader) -> Option<Elsewhere> {
    let observed = session::owner::observe(reader);
    let recorded = match &observed {
        session::owner::OwnerRecord::Recorded(recorded) => Some(recorded),
        _ => None,
    };
    HandleRecords::read(reader).elsewhere(session_id(reader), recorded)
}

#[cfg(test)]
mod tests;
