//! Whether a Session belongs to another OS, judged from its Records alone.
//!
//! A state directory can be copied or moved between machines. A Session belongs to another OS
//! when its terminal handle names a kind this OS does not run, or when its Owner record carries
//! the identity that another OS's writer records (macOS: the process start time; Linux: the
//! Linux birth; Windows: the Windows process identity). This machine's processes are no
//! evidence about such a Session: its owner is never attested here (`session::owner`), Repair
//! leaves it alone, and every command that would act on it gets one refusal, the same on every
//! OS: the kind's refusal (`OsTerminals::unavailable`) when the handle names a kind this OS does
//! not run, otherwise `a session recorded on <OS> is not available on <this OS>`. What its
//! Records alone settle (an interrupted close, a tombstone, no handle at all) still settles.
//! An Owner record that names no identity (a legacy PID-only record) is judged by its handle.
use super::ownership::NativeSessionOwner;
use super::{OsTerminals, TerminalKind, TerminalSession};
use crate::native::failure_kind::Unsupported;
use crate::native::session::{self, CoreRecord, Reader};

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

/// Whether this Owner record carries an identity that this OS does not record, and so cannot
/// attest: it was written on another OS (or names several).
pub(in crate::native) fn owner_recorded_elsewhere(owner: &NativeSessionOwner) -> bool {
    OsTerminals::CURRENT.is_some_and(|os| {
        recorded_on(owner)
            .iter()
            .any(|recorded| recorded.os != os.os)
    })
}

/// The one refusal for a Session that belongs to another OS, from the handle the caller holds
/// (or none) and the Owner record; `None` for a Session of this OS.
pub(in crate::native) fn judge(
    handle: Option<&TerminalSession>,
    owner: Option<&NativeSessionOwner>,
) -> Option<anyhow::Error> {
    let os = OsTerminals::CURRENT?;
    if let Some(handle) = handle
        && !os.runs(handle.kind)
    {
        return Some(os.unavailable(handle.kind));
    }
    let owner = owner.filter(|owner| owner_recorded_elsewhere(owner))?;
    let origin = match recorded_on(owner).as_slice() {
        [one] => one.os,
        _ => "more than one operating system",
    };
    Some(anyhow::Error::new(Unsupported::new(format!(
        "a session recorded on {origin} is not available on {}",
        os.os
    ))))
}

/// The same judgment from the Session's Records: the given handle, or else the active handle
/// and then a claimed one, and the Owner record. A Record that cannot be read is no evidence
/// either way; its own reader reports it.
pub(in crate::native) fn of_records(
    reader: &Reader,
    handle: Option<&TerminalSession>,
) -> Option<anyhow::Error> {
    let read = handle.is_none().then(|| recorded_handle(reader)).flatten();
    let owner = match session::owner::observe(reader) {
        session::owner::OwnerRecord::Recorded(recorded) => Some(recorded.owner),
        _ => None,
    };
    judge(handle.or(read.as_ref()), owner.as_ref())
}

/// Repair's judgment: only an Owner record of another OS keeps Repair from attesting it here,
/// and then the refusal names the handle's kind when this OS does not run it.
pub(in crate::native) fn owner_of_another_os(reader: &Reader) -> Option<anyhow::Error> {
    let session::owner::OwnerRecord::Recorded(recorded) = session::owner::observe(reader) else {
        return None;
    };
    if !owner_recorded_elsewhere(&recorded.owner) {
        return None;
    }
    judge(recorded_handle(reader).as_ref(), Some(&recorded.owner))
}

/// The refusal of a kind this OS does not run, for readers outside the terminal module.
pub(in crate::native) fn kind_refusal(kind: TerminalKind) -> Option<anyhow::Error> {
    OsTerminals::CURRENT
        .filter(|os| !os.runs(kind))
        .map(|os| os.unavailable(kind))
}

fn recorded_handle(reader: &Reader) -> Option<TerminalSession> {
    [CoreRecord::Terminal, CoreRecord::TerminalClosing]
        .into_iter()
        .find_map(|record| {
            reader
                .record(record)
                .optional_json_with(session::record::terminal)
                .ok()
                .flatten()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::failure_kind::{FailureKind, classify};

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

    // Each OS's own Owner shape is its own; every other OS's shape, and a mix, belongs to
    // another OS. A PID-only record names no OS. The handle's kind answers first.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn a_session_of_another_os_is_judged_by_its_handle_then_its_owner() {
        let here = OsTerminals::CURRENT.unwrap().os;
        let legacy = NativeSessionOwner {
            pid: 4242,
            ..Default::default()
        };
        assert!(!owner_recorded_elsewhere(&legacy));
        assert!(judge(None, Some(&legacy)).is_none());
        for os in ["macOS", "Linux", "Windows"] {
            let owner = owner_of(os);
            assert_eq!(owner_recorded_elsewhere(&owner), os != here, "{os}");
            let herdr = TerminalSession::new(TerminalKind::Herdr, "t7".into());
            match judge(Some(&herdr), Some(&owner)) {
                None => assert_eq!(os, here),
                Some(error) => {
                    assert_eq!(
                        format!("{error:#}"),
                        format!("a session recorded on {os} is not available on {here}")
                    );
                    assert_eq!(classify(&error), Some(FailureKind::Unsupported));
                }
            }
            for kind in TerminalKind::ALL {
                let handle = TerminalSession::new(kind, "surface".into());
                let judged = judge(Some(&handle), Some(&owner)).map(|error| format!("{error:#}"));
                if !kind.supported_on_this_platform() {
                    assert_eq!(
                        judged,
                        kind_refusal(kind).map(|error| format!("{error:#}")),
                        "{kind:?}"
                    );
                }
            }
        }
        let mut mixed = owner_of("Linux");
        mixed.windows_process_identity = owner_of("Windows").windows_process_identity;
        assert_eq!(
            format!("{:#}", judge(None, Some(&mixed)).unwrap()),
            format!(
                "a session recorded on more than one operating system is not available on {here}"
            )
        );
    }
}
