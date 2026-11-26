//! Whether a Session belongs to another OS, judged from its Records alone, in one place.
//!
//! A state directory can be copied or moved between machines. A Session belongs to another OS
//! when any of its Records says so positively: the kind of its active, claimed or consumed
//! handle is one this OS does not run, a Herdr handle's recorded host birth names another OS,
//! or its Owner record carries the identity another OS's writer records (macOS: the process
//! start time; Linux: the Linux birth; Windows: the Windows process identity). A missing or
//! null field is no evidence either way, and a PID-only or absent Owner record does not hide
//! a terminal of another OS.
//!
//! The judgment rides in the shared steps rather than in each command:
//! - the Owner record's one observation (`session::owner::observe`, `required`, and Origin's
//!   read) carries the handle Records' verdict on the owner, and the one attestation
//!   (`attest_owner_with`) makes and reports no local process observation for such an owner;
//! - Repair (`repair_dead_owner_with`) leaves such a session exactly as it is;
//! - the shared surface judgment (`judgment::surface`) and the ownership check
//!   (`verified_terminal_surface_owner`) refuse its handle, and each OS's stored-handle lookup
//!   refuses a kind it does not run, before any adapter.
//!
//! Every command admits a session or its handle in one order: a closed session first, then a
//! handle that is not this session's, then a session of another OS. The refusal is the same on
//! every OS (`Elsewhere::refusal`).
use super::ownership::NativeSessionOwner;
use super::record::HerdrHostBirth;
use super::{OsTerminals, TerminalKind, TerminalSession};
use crate::native::failure_kind::Unsupported;
use crate::native::session::{self, CoreRecord, Reader};
use std::path::Path;

/// What a Session's Records name as another OS, with the refusal it gives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum Elsewhere {
    /// A handle (active, claimed or consumed) names a kind this OS does not run.
    Kind(TerminalKind),
    /// A Herdr handle's host birth or the Owner record's identity names another OS (or
    /// several).
    Recorded(&'static str),
}

impl Elsewhere {
    /// What the Records name, the same on every OS: the kind's refusal when a handle names a
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
    /// command that would act on it. It says what is true: nothing on this machine can judge
    /// or end the session, and an explicit close settles it once its Records show it ended.
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

/// The OS whose Herdr host the handle's recorded birth describes.
fn birth_os(birth: &HerdrHostBirth) -> OsTerminals {
    match birth {
        HerdrHostBirth::Macos { .. } => OsTerminals::MACOS,
        HerdrHostBirth::Linux { .. } => OsTerminals::LINUX,
        HerdrHostBirth::Windows { .. } => OsTerminals::WINDOWS,
    }
}

/// What a handle itself says: its kind, then a Herdr handle's recorded host birth.
pub(in crate::native) fn of_handle(handle: &TerminalSession) -> Option<Elsewhere> {
    let os = OsTerminals::CURRENT?;
    if !os.runs(handle.kind) {
        return Some(Elsewhere::Kind(handle.kind));
    }
    let born = birth_os(&handle.herdr_core()?.birth);
    (born.os != os.os).then_some(Elsewhere::Recorded(born.os))
}
/// What an Owner record itself says: an identity this OS does not record.
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

/// What the handle Records say, without the Owner record: the active handle, else a claimed
/// one, else the kind the close tombstone kept. A Record that cannot be read, and a missing or
/// null kind, are no evidence.
pub(in crate::native) fn of_surface_records(reader: &Reader) -> Option<Elsewhere> {
    for record in [CoreRecord::Terminal, CoreRecord::TerminalClosing] {
        if let Ok(Some(handle)) = reader
            .record(record)
            .optional_json_with(session::record::terminal)
        {
            return of_handle(&handle);
        }
    }
    let tombstone = reader
        .record(CoreRecord::TerminalClosed)
        .optional_json_with(session::record::terminal_tombstone_value)
        .ok()
        .flatten()?;
    let kind: TerminalKind = serde_json::from_value(tombstone.get("terminal")?.clone()).ok()?;
    let os = OsTerminals::CURRENT?;
    (!os.runs(kind)).then_some(Elsewhere::Kind(kind))
}

/// The whole judgment of a Session from its Records: the handle the caller holds (or the
/// recorded ones), then the Owner record as its one observation reads it.
pub(in crate::native) fn of_records(
    reader: &Reader,
    handle: Option<&TerminalSession>,
) -> Option<Elsewhere> {
    let surface = match handle {
        Some(handle) => of_handle(handle),
        None => of_surface_records(reader),
    };
    surface.or_else(|| of_owner_record(&session::owner::observe(reader)))
}

/// The judgment of an active handle with the Owner record read beside it, as the shared
/// surface judgment makes it (`judgment::surface`).
pub(in crate::native) fn of_handle_and_owner(
    handle: &TerminalSession,
    owner: &session::owner::OwnerRecord,
) -> Option<Elsewhere> {
    of_handle(handle).or_else(|| of_owner_record(owner))
}

fn of_owner_record(owner: &session::owner::OwnerRecord) -> Option<Elsewhere> {
    match owner {
        session::owner::OwnerRecord::Recorded(recorded) => of_owner(&recorded.owner),
        _ => None,
    }
}

/// Whether the Records say the session is closed: its status, or the closed status record
/// that an interrupted close can leave before `status.json` catches up. A closed session
/// answers first, before its handle's binding and the OS it belongs to, for every command.
pub(in crate::native) fn closed(reader: &Reader) -> bool {
    reader
        .status()
        .is_ok_and(|status| status.state == session::SessionState::Closed)
        || reader.closed_if_present().ok().flatten().is_some()
}

/// A session's admission for a command that changes it without addressing its terminal
/// (`hold`, `cancel`): after the command's own closed answer, a session of another OS is
/// refused. Such a command has no handle to bind, so it has no binding step.
pub(in crate::native) fn admit_session(reader: &Reader) -> anyhow::Result<()> {
    match of_records(reader, None) {
        Some(elsewhere) => Err(elsewhere.refusal(reader.directory())),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::failure_kind::{FailureKind, classify};
    use crate::native::session::Store;

    fn owner_of(os: &str) -> NativeSessionOwner {
        let mut owner = NativeSessionOwner {
            pid: 4242,
            ..Default::default()
        };
        match os {
            "macOS" => {
                owner.process_start_seconds = Some(1);
                owner.process_start_microseconds = Some(2);
            }
            "Linux" => {
                owner.linux_birth = Some(
                    serde_json::from_value(serde_json::json!({
                        "boot_id": "00112233-4455-6677-8899-aabbccddeeff",
                        "starttime_ticks": 7
                    }))
                    .unwrap(),
                );
            }
            _ => {
                owner.windows_process_identity =
                    Some(crate::native::terminal::WindowsProcessIdentity {
                        creation_time: 1,
                        executable_path: r"C:\Program Files\PowerShell\7\pwsh.exe".into(),
                    });
            }
        }
        owner
    }

    fn herdr_born_on(os: &str) -> TerminalSession {
        let birth = match os {
            "macOS" => serde_json::json!({"host_start_seconds": 1, "host_start_microseconds": 2}),
            "Linux" => serde_json::json!({"linux_birth": {
                "boot_id": "00112233-4455-6677-8899-aabbccddeeff", "starttime_ticks": 7}}),
            _ => serde_json::json!({"host_creation_time": 1}),
        };
        let mut core = serde_json::json!({"socket": "/tmp/core.sock", "core_id": "core-a",
            "created_ms": 1, "host_pid": 4});
        for (key, value) in birth.as_object().unwrap() {
            core[key] = value.clone();
        }
        serde_json::from_value(serde_json::json!({"terminal": "herdr", "session_id": "t7",
            "managed_session_id": "session-foreign", "herdr_core": core}))
        .unwrap()
    }

    // Each kind of evidence on its own: a handle's kind, a Herdr host birth, an Owner
    // identity; their refusals; and a PID-only owner or a Herdr handle without a birth as no
    // evidence.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn each_record_names_its_os_and_a_missing_identity_names_none() {
        let here = OsTerminals::CURRENT.unwrap().os;
        let legacy = NativeSessionOwner {
            pid: 4242,
            ..Default::default()
        };
        assert_eq!(of_owner(&legacy), None);
        assert_eq!(
            of_handle(&TerminalSession::new(TerminalKind::Herdr, "t7".into())),
            None
        );
        for os in ["macOS", "Linux", "Windows"] {
            let expected = (os != here).then_some(Elsewhere::Recorded(
                [OsTerminals::MACOS, OsTerminals::LINUX, OsTerminals::WINDOWS]
                    .into_iter()
                    .find(|system| system.os == os)
                    .unwrap()
                    .os,
            ));
            assert_eq!(of_owner(&owner_of(os)), expected, "{os}");
            assert_eq!(of_handle(&herdr_born_on(os)), expected, "{os}");
            if let Some(elsewhere) = expected {
                let error = elsewhere.refusal(Path::new("records"));
                assert_eq!(
                    format!("{error:#}"),
                    format!(
                        "a session recorded on {os} is not available on {here}: TAB on this \
                         machine cannot judge or end this session; once its records show it \
                         ended, an explicit close settles it (its records are in records)"
                    )
                );
                assert_eq!(classify(&error), Some(FailureKind::Unsupported));
            }
        }
        for kind in TerminalKind::ALL {
            let judged = of_handle(&TerminalSession::new(kind, "surface".into()));
            if kind.supported_on_this_platform() {
                assert_eq!(judged, None, "{kind:?}");
            } else {
                assert_eq!(judged, Some(Elsewhere::Kind(kind)), "{kind:?}");
                assert_eq!(
                    judged.unwrap().reason(),
                    format!("{} is not available on {here}", kind.display_name())
                );
            }
        }
        let mut mixed = owner_of("Linux");
        mixed.windows_process_identity = owner_of("Windows").windows_process_identity;
        assert_eq!(
            of_owner(&mixed),
            Some(Elsewhere::Recorded("more than one operating system"))
        );
    }

    // The Records in their precedence: the active handle, a claimed one, the kind a close
    // tombstone kept (a missing or null kind is no evidence), then the Owner record.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn the_records_are_read_in_one_precedence() {
        let foreign = TerminalKind::ALL
            .into_iter()
            .find(|kind| !kind.supported_on_this_platform())
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-foreign");
        std::fs::create_dir(&directory).unwrap();
        let store = Store::open_unchecked(&directory);
        let reader = Reader::open_unchecked(&directory);
        assert_eq!(of_records(&reader, None), None);
        for tombstone in [
            serde_json::json!({"consumed": true}),
            serde_json::json!({"consumed": true, "terminal": null}),
        ] {
            store
                .record(CoreRecord::TerminalClosed)
                .write_json(&tombstone)
                .unwrap();
            assert_eq!(of_records(&reader, None), None, "{tombstone}");
        }
        store
            .record(CoreRecord::TerminalClosed)
            .write_json(&serde_json::json!({"consumed": true, "terminal": foreign}))
            .unwrap();
        assert_eq!(of_records(&reader, None), Some(Elsewhere::Kind(foreign)));
        store
            .record(CoreRecord::TerminalClosing)
            .write_json(&TerminalSession::new(TerminalKind::Herdr, "t7".into()))
            .unwrap();
        // A handle of this OS's kind decides before the tombstone.
        assert_eq!(of_records(&reader, None), None);
        let other = ["macOS", "Linux", "Windows"]
            .into_iter()
            .find(|os| *os != OsTerminals::CURRENT.unwrap().os)
            .unwrap();
        store.write_owner(&owner_of(other)).unwrap();
        assert!(matches!(
            of_records(&reader, None),
            Some(Elsewhere::Recorded(_))
        ));
    }
}
