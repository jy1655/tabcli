//! Whether a Session belongs to another OS, from its own Records, in one place.
//!
//! A state directory can be copied or moved between machines. A Session belongs to another OS
//! only on a positive statement in a Record that is this Session's own:
//! - its active handle, bound to this Session and stating its kind, names a kind this OS does
//!   not run, or, for a Herdr handle only, records a host birth of another platform's variant;
//! - its consumption marker says `consumed: true` and names a kind this OS does not run;
//! - its Owner record, bound to this Session, carries the identity another OS's writer records
//!   (macOS: the process start time; Linux: the Linux birth; Windows: the Windows process
//!   identity).
//!
//! Nothing else is evidence: the kind the reader supplies for a handle that omits it (iTerm2,
//! the legacy default), a field that does not belong to the handle's kind, a Record bound to
//! another Session or to none, a marker that does not decode or does not say `consumed: true`,
//! a claimed handle, and anything missing, null or unreadable. Without evidence the Session is
//! this OS's, and every command acts exactly as it always did.
//!
//! When the Records name more than one OS, the first statement in this order gives the
//! refusal's opening: the handle a command read and is about to use (`admit` only), the active
//! handle, the consumption marker, then the Owner record.
//!
//! For a Session with evidence, TAB does nothing that depends on this machine's terminals or
//! processes. Each shared step that would observe them takes one early branch on this
//! judgment, complete in itself, and leaves the rest of the step as it was: the Owner record's
//! one observation (`session::owner::observe`, `required`) marks the owner, so owner
//! attestation (`attest_owner_with`) observes nothing and launch repair decides nothing from
//! it; Repair leaves the Session as it is; prune does not count a closed copy's owner; Origin
//! does not attest it; a refused reopen's provider process is not observed; and each command
//! that would address its terminal (`tell`'s ownership proof, `screen`, doctor's probe, the
//! close of a claimed handle) refuses it in one order (`admit`): a closed Session, then a
//! handle that is not this Session's, then another OS. The close transaction settles such a
//! Session from its Records alone when they show it ended (`ended`), and refuses any other.
//! Everything TAB does from Records alone is unchanged.
use super::ownership::NativeSessionOwner;
use super::{OsTerminals, TerminalKind, TerminalSession};
use crate::native::failure_kind::Unsupported;
use crate::native::session::{self, CoreRecord, Reader};
use serde_json::Value;
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

/// What a handle itself says: a kind this OS does not run, or, for a Herdr handle only, a
/// recorded host birth of another platform's variant. It does not say whose handle it is.
pub(in crate::native) fn of_handle(handle: &TerminalSession) -> Option<Elsewhere> {
    of_handle_on(OsTerminals::CURRENT?, handle)
}

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

/// What an Owner record's identity says: an identity this OS does not record. Evidence only
/// for a record bound to the session.
pub(in crate::native) fn of_owner(owner: &NativeSessionOwner) -> Option<Elsewhere> {
    of_owner_on(OsTerminals::CURRENT?, owner)
}

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

/// Whether a decoded handle stated its kind. The reader supplies iTerm2 for a handle record
/// that omits its kind (the legacy default), and only iTerm2, so a decoded handle of any other
/// kind stated it; a decoded iTerm2 handle is evidence only from its record's bytes
/// (`stated_handle`).
fn states_kind(handle: &TerminalSession) -> bool {
    handle.kind != TerminalKind::Iterm2
}

/// The handle a record's bytes hold, only when the record itself states its kind (a
/// `terminal` field) and decodes.
fn stated_handle(bytes: &[u8]) -> Option<TerminalSession> {
    let text = std::str::from_utf8(bytes).ok()?;
    let fields: serde_json::Map<String, Value> = serde_json::from_str(text).ok()?;
    if !fields.contains_key("terminal") {
        return None;
    }
    session::record::terminal(text).ok()
}

/// A consumption marker that says its consumption completed.
#[derive(Debug, Eq, PartialEq)]
pub(in crate::native) struct Consumption {
    /// The consumed kind, when the marker names one this version knows.
    pub(in crate::native) kind: Option<TerminalKind>,
    /// The close's reason, when the marker records one (macOS and Linux writers do).
    pub(in crate::native) reason: Option<String>,
}

/// What a consumption marker's bytes say: `consumed: true` alone says that a consumption
/// completed; the kind it names and its reason are optional. `None` for bytes that are not a
/// JSON object saying `consumed: true`.
pub(in crate::native) fn consumption(marker: &[u8]) -> Option<Consumption> {
    let value: Value = serde_json::from_str(std::str::from_utf8(marker).ok()?).ok()?;
    if value.get("consumed") != Some(&Value::Bool(true)) {
        return None;
    }
    Some(Consumption {
        kind: value
            .get("terminal")
            .and_then(|kind| serde_json::from_value(kind.clone()).ok()),
        reason: value
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// What the handle Records of `session` say, from their bytes: the bound active handle that
/// states its kind, then the consumption marker. A Record that does not decode is no
/// evidence. Readers that hold their own bytes (Origin's budgeted read) judge them here.
pub(in crate::native) fn of_handle_record_bytes(
    session: &str,
    handle: Option<&[u8]>,
    marker: Option<&[u8]>,
) -> Option<Elsewhere> {
    of_handle_record_bytes_on(OsTerminals::CURRENT?, session, handle, marker)
}

fn of_handle_record_bytes_on(
    os: OsTerminals,
    session: &str,
    handle: Option<&[u8]>,
    marker: Option<&[u8]>,
) -> Option<Elsewhere> {
    if let Some(handle) = handle.and_then(stated_handle)
        && bound_here(&handle, session)
        && let Some(elsewhere) = of_handle_on(os, &handle)
    {
        return Some(elsewhere);
    }
    let kind = consumption(marker?)?.kind?;
    (!os.runs(kind)).then_some(Elsewhere::Kind(kind))
}

/// The judgment of the Session `reader` opens, from its own Records: its handle Records, then
/// its Owner record if it is bound to the Session. A Record that cannot be read is no
/// evidence, so this read never fails and never changes what a command does for a Session of
/// this OS.
pub(in crate::native) fn of_records(reader: &Reader) -> Option<Elsewhere> {
    of_records_on(OsTerminals::CURRENT?, reader)
}

fn of_records_on(os: OsTerminals, reader: &Reader) -> Option<Elsewhere> {
    let bytes = |record| reader.record(record).bytes().ok().flatten();
    of_handle_record_bytes_on(
        os,
        session_of(reader),
        bytes(CoreRecord::Terminal).as_deref(),
        bytes(CoreRecord::TerminalClosed).as_deref(),
    )
    .or_else(|| session::owner::bound(reader).and_then(|owner| of_owner_on(os, &owner)))
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
/// Session `reader` opens, addressing `handle` if it has one. The handle the command read and
/// is about to use is judged as it was read, so that a Record changed since then (a
/// settlement that consumed it) cannot let it through; then the Session's Records. `Ok` when
/// neither gives evidence of another OS: the command's own path applies unchanged. Otherwise
/// the one refusal, in one order: a closed Session (`closed`, the command's own text), then a
/// handle that is not this Session's, then the Session of another OS.
pub(in crate::native) fn admit(
    reader: &Reader,
    handle: Option<&TerminalSession>,
    closed_answer: impl FnOnce() -> anyhow::Error,
) -> anyhow::Result<()> {
    let session = session_of(reader);
    let carried = handle
        .filter(|handle| states_kind(handle) && bound_here(handle, session))
        .and_then(of_handle);
    let Some(elsewhere) = carried.or_else(|| of_records(reader)) else {
        return Ok(());
    };
    if closed(reader) {
        return Err(closed_answer());
    }
    if let Some(handle) = handle {
        binding(handle, session)?;
    }
    Err(elsewhere.refusal(reader.directory()))
}

#[cfg(test)]
mod tests;
