//! Whether a Session belongs to another OS, from its own Records, in one place.
//!
//! A state directory can be copied or moved between machines. A Session belongs to another OS
//! only on a positive statement in a Record that is this Session's own:
//! - its active handle, bound to this Session, names a kind this OS does not run, or, for a
//!   Herdr handle only, records a host birth of another platform's variant;
//! - its consumption marker decodes, says `consumed: true` and names a kind this OS does not
//!   run;
//! - its Owner record, bound to this Session, carries the identity another OS's writer records
//!   (macOS: the process start time; Linux: the Linux birth; Windows: the Windows process
//!   identity).
//!
//! Nothing else is evidence: a field that does not belong to the handle's kind, a Record bound
//! to another Session or to none, a marker that does not decode or says `consumed: false`, a
//! claimed handle, and anything missing or null. Without evidence the Session is this OS's, and
//! every command acts as it always did.
//!
//! For a Session with evidence, TAB does nothing that depends on this machine's terminals or
//! processes: the Owner record's one observation (`session::owner::observe`, `required`,
//! Origin's budgeted read) carries the verdict, so owner attestation observes nothing and
//! Repair leaves the Session as it is; each command that would address its terminal (`tell`'s
//! ownership proof, `screen`, doctor's probe, the explicit close) refuses it in one order
//! (`admit`): a closed Session, then a handle that is not this Session's, then another OS.
//! The explicit close settles such a Session from its Records alone when they show it ended
//! (`ended`), and prune removes it once closed. Everything TAB does from Records alone is
//! unchanged.
use super::ownership::NativeSessionOwner;
use super::{OsTerminals, TerminalKind, TerminalSession};
use crate::native::failure_kind::Unsupported;
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

    /// The one refusal of a session of another OS whose Records are in `directory`, for every
    /// command that would use this machine's terminal or processes for it. It says what is
    /// true: nothing here can judge or end the session, and an explicit close settles it once
    /// its Records show it ended.
    pub(in crate::native) fn refusal(self, directory: &Path) -> anyhow::Error {
        anyhow::Error::new(Unsupported::new(format!(
            "{}: TAB on this machine cannot judge or end this session; once its records show \
             it ended, an explicit close settles it (its records are in {})",
            self.head(),
            directory.display()
        )))
    }

    /// The reason an owner observation reports for such an owner: what the Records name.
    pub(in crate::native) fn reason(self) -> String {
        self.head()
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

/// What a handle itself says: a kind this OS does not run, or, for a Herdr handle only, a
/// recorded host birth of another platform's variant. It does not say whose handle it is.
pub(in crate::native) fn of_handle(handle: &TerminalSession) -> Option<Elsewhere> {
    let os = OsTerminals::CURRENT?;
    if !os.runs(handle.kind) {
        return Some(Elsewhere::Kind(handle.kind));
    }
    if handle.kind != TerminalKind::Herdr {
        return None;
    }
    let born = herdr_birth_os(handle)?;
    (born.os != os.os).then_some(Elsewhere::Recorded(born.os))
}

/// What an Owner record's identity says: an identity this OS does not record. Evidence only
/// for a record bound to the session (`of_owner_record`).
pub(in crate::native) fn of_owner(owner: &NativeSessionOwner) -> Option<Elsewhere> {
    let os = OsTerminals::CURRENT?;
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

/// The kind a consumption marker says was consumed: only a marker that decodes and says
/// `consumed: true`. `None` for a marker that is absent, does not decode, says
/// `consumed: false`, or names no kind.
pub(in crate::native) fn consumed_kind(marker: &[u8]) -> Option<TerminalKind> {
    let tombstone = session::record::terminal_tombstone(std::str::from_utf8(marker).ok()?).ok()?;
    tombstone.terminal.filter(|_| tombstone.consumed)
}

/// What the handle Records of `session` say, from their bytes: the bound active handle, then
/// the consumption marker. A Record that does not decode is no evidence. Readers that hold
/// their own bytes (Origin's budgeted read) judge them here.
pub(in crate::native) fn of_handle_record_bytes(
    session: &str,
    handle: Option<&[u8]>,
    marker: Option<&[u8]>,
) -> Option<Elsewhere> {
    if let Some(handle) = handle
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| session::record::terminal(text).ok())
        && bound_here(&handle, session)
        && let Some(elsewhere) = of_handle(&handle)
    {
        return Some(elsewhere);
    }
    let kind = consumed_kind(marker?)?;
    (!OsTerminals::CURRENT?.runs(kind)).then_some(Elsewhere::Kind(kind))
}

/// What the handle Records of the Session `reader` opens say. Read by the Owner record's one
/// observation, which adds what a bound Owner record says.
pub(in crate::native) fn of_handle_records(reader: &Reader) -> Option<Elsewhere> {
    let bytes = |record| reader.record(record).bytes().ok().flatten();
    of_handle_record_bytes(
        session_of(reader),
        bytes(CoreRecord::Terminal).as_deref(),
        bytes(CoreRecord::TerminalClosed).as_deref(),
    )
}

/// What a decoded Owner record adds to the handle Records' verdict: its identity, only when it
/// is bound to the Session.
pub(in crate::native) fn with_owner(
    handle_records: Option<Elsewhere>,
    owner: &NativeSessionOwner,
    binding: super::ownership::OwnerBinding,
) -> Option<Elsewhere> {
    handle_records.or_else(|| {
        (binding == super::ownership::OwnerBinding::Bound)
            .then(|| of_owner(owner))
            .flatten()
    })
}

/// The judgment of the Session `reader` opens, from its own Records: what its Owner record's
/// one observation carries, or the handle Records alone when there is no decodable Owner
/// record.
pub(in crate::native) fn of_records(reader: &Reader) -> Option<Elsewhere> {
    match session::owner::observe(reader) {
        session::owner::OwnerRecord::Recorded(recorded) => recorded.owner.recorded_elsewhere,
        _ => of_handle_records(reader),
    }
}

fn session_of(reader: &Reader) -> &str {
    reader
        .directory()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
}

/// Whether the Records say the session is closed: its status, or the closed status record
/// that an interrupted close can leave before `status.json` catches up.
pub(in crate::native) fn closed(reader: &Reader) -> bool {
    reader
        .status()
        .is_ok_and(|status| status.state == session::SessionState::Closed)
        || reader.closed_if_present().ok().flatten().is_some()
}

/// The binding a command that addresses `handle` requires, in the order's second place: the
/// handle names this session (a legacy iTerm2 handle that names none counts as this
/// session's) and has a surface id.
pub(in crate::native) fn binding(handle: &TerminalSession, session: &str) -> anyhow::Result<()> {
    handle.verify_managed_session(session)?;
    if handle.id.is_empty() {
        anyhow::bail!("the recorded terminal handle is not bound to session {session}");
    }
    Ok(())
}

/// The admission of a command that would use this machine's terminal or processes for the
/// Session `reader` opens, addressing `handle` if it has one. `Ok` when the Session's Records
/// give no evidence of another OS: the command's own path applies unchanged. Otherwise the one
/// refusal, in one order: a closed Session (`closed`, the command's own text), then a handle
/// that is not this Session's, then the Session of another OS.
pub(in crate::native) fn admit(
    reader: &Reader,
    handle: Option<&TerminalSession>,
    closed_answer: impl FnOnce() -> anyhow::Error,
) -> anyhow::Result<()> {
    let Some(elsewhere) = of_records(reader) else {
        return Ok(());
    };
    if closed(reader) {
        return Err(closed_answer());
    }
    if let Some(handle) = handle {
        binding(handle, session_of(reader))?;
    }
    Err(elsewhere.refusal(reader.directory()))
}

#[cfg(test)]
mod tests;
