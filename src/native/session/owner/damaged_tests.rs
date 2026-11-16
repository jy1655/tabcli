//! Owner records no writer produces (copied, tampered or damaged session directories), and
//! what each lifecycle reader of the Owner record answers for them.
use super::*;
use crate::native::process_identity::Gone;
use crate::native::session::Store;
use std::fs;
use std::path::{Path, PathBuf};

/// One byte over the Owner record's read bound.
pub(in crate::native) const OVER_BOUND: usize = 128 * 1024 + 1;

/// A shape of `native-session.json` that no writer produces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum Damaged {
    /// A symlink at the final component to a valid record beside it.
    #[cfg(unix)]
    Linked,
    /// A symlink to nothing.
    #[cfg(unix)]
    Dangling,
    /// A directory at the record path.
    Directory,
    /// A valid record with one byte that is not UTF-8 inside a JSON string.
    NonUtf8,
    /// A valid record padded with whitespace to one byte over 128 KiB.
    Oversized,
}

pub(in crate::native) const DAMAGED: &[Damaged] = &[
    #[cfg(unix)]
    Damaged::Linked,
    #[cfg(unix)]
    Damaged::Dangling,
    Damaged::Directory,
    Damaged::NonUtf8,
    Damaged::Oversized,
];

pub(in crate::native) fn owner_path(directory: &Path) -> PathBuf {
    Reader::open_unchecked(directory)
        .record(CoreRecord::Owner)
        .path()
        .to_owned()
}

/// Writes `owner` at the session's owner path in the `damaged` shape, replacing any record.
pub(in crate::native) fn write_damaged_owner(
    directory: &Path,
    owner: &NativeSessionOwner,
    damaged: Damaged,
) {
    let path = owner_path(directory);
    let _ = fs::remove_file(&path);
    match damaged {
        #[cfg(unix)]
        Damaged::Linked => {
            let target = directory.join("owner-link-target.json");
            fs::write(&target, serde_json::to_vec(owner).unwrap()).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
        }
        #[cfg(unix)]
        Damaged::Dangling => {
            std::os::unix::fs::symlink(directory.join("owner-link-missing.json"), &path).unwrap()
        }
        Damaged::Directory => fs::create_dir(&path).unwrap(),
        Damaged::NonUtf8 => {
            let mut marked = owner.clone();
            marked.terminal_tty = Some("NONUTF8".to_owned());
            let text = serde_json::to_string(&marked).unwrap();
            let (before, after) = text.split_once("NONUTF8").unwrap();
            let mut bytes = before.as_bytes().to_vec();
            bytes.push(0xff);
            bytes.extend_from_slice(after.as_bytes());
            fs::write(&path, bytes).unwrap();
        }
        Damaged::Oversized => {
            let mut bytes = serde_json::to_vec_pretty(owner).unwrap();
            bytes.resize(OVER_BOUND, b' ');
            fs::write(&path, bytes).unwrap();
        }
    }
}

/// What every reader reports for a record the read policy refuses.
pub(in crate::native) fn read_error(path: &Path, damaged: Damaged) -> String {
    match damaged {
        Damaged::NonUtf8 => format!(
            "failed to read {}: stream did not contain valid UTF-8",
            path.display()
        ),
        Damaged::Oversized => format!(
            "record is {OVER_BOUND} bytes, over the 131072 byte read limit: {}",
            path.display()
        ),
        _ => format!("refusing non-regular session file: {}", path.display()),
    }
}

fn owner(pid: u32, session: Option<&str>) -> NativeSessionOwner {
    NativeSessionOwner {
        pid,
        managed_session_id: session.map(str::to_owned),
        ..NativeSessionOwner::default()
    }
}

/// What a lifecycle reader answered: the error text, or the outcome it reached.
fn outcome<T: std::fmt::Debug>(result: Result<T>) -> String {
    match result {
        Ok(value) => format!("ok {value:?}"),
        Err(error) => format!("error {error:#}"),
    }
}

// Launch repair reads the owner to attest the recorded process before startup is confirmed.
#[test]
fn launch_repair_owner_read_answers_for_each_damaged_shape() {
    for &damaged in DAMAGED {
        let root = tempfile::tempdir().unwrap();
        let path = owner_path(root.path());
        write_damaged_owner(root.path(), &owner(std::process::id(), None), damaged);
        let observed = outcome(
            attest_recorded_owner(&Reader::open_unchecked(root.path()))
                .map(|a| a.map(|a| matches!(a.process, ProcessObservation::Live(_)))),
        );
        assert_eq!(
            observed,
            format!("error {}", read_error(&path, damaged)),
            "{damaged:?}"
        );
    }
}

// The macOS running wait attests the recorded owner on every poll; an attested death ends it.
#[cfg(target_os = "macos")]
#[test]
fn running_wait_owner_read_answers_for_each_damaged_shape() {
    for &damaged in DAMAGED {
        let root = tempfile::tempdir().unwrap();
        let path = owner_path(root.path());
        write_damaged_owner(root.path(), &owner(4242, None), damaged);
        let observed = outcome(crate::native::session::turn::running_wait_owner_with(
            root.path(),
            |_| OwnerAttestation {
                process: ProcessObservation::Gone(Gone::Exited),
                control: ControlAttestation::NotRecorded,
                pid_alive: Some(false),
            },
        ));
        assert_eq!(
            observed,
            format!("error {}", read_error(&path, damaged)),
            "{damaged:?}"
        );
    }
}

// The required owner read of the macOS close paths (teardown intent, close authority and the
// Terminal.app close).
#[cfg(target_os = "macos")]
#[test]
fn required_owner_read_answers_for_each_damaged_shape_and_a_missing_record() {
    let root = tempfile::tempdir().unwrap();
    let path = owner_path(root.path());
    assert_eq!(
        outcome(Reader::open_unchecked(root.path()).owner().map(|o| o.pid)),
        format!(
            "error failed to read {}: No such file or directory (os error 2)",
            path.display()
        )
    );
    for &damaged in DAMAGED {
        let root = tempfile::tempdir().unwrap();
        let path = owner_path(root.path());
        write_damaged_owner(root.path(), &owner(4242, Some("session-a")), damaged);
        let observed = outcome(Reader::open_unchecked(root.path()).owner().map(|o| o.pid));
        assert_eq!(
            observed,
            format!("error {}", read_error(&path, damaged)),
            "{damaged:?}"
        );
    }
}

// The macOS surface-ownership probe `tell` runs: it reads the owner, then verifies its
// binding and control identity against the live process.
#[cfg(target_os = "macos")]
#[test]
fn surface_ownership_probe_owner_read_answers_for_each_damaged_shape() {
    use crate::native::terminal::macos::ownership::verified_macos_terminal_owner;
    let session: crate::native::terminal::TerminalSession = serde_json::from_value(
        serde_json::json!({"terminal": "apple-terminal", "session_id": "/dev/ttys999",
            "window_id": "1001", "managed_session_id": "session-a"}),
    )
    .unwrap();
    for &damaged in DAMAGED {
        let root = tempfile::tempdir().unwrap();
        let path = owner_path(root.path());
        write_damaged_owner(
            root.path(),
            &owner(std::process::id(), Some("session-a")),
            damaged,
        );
        let observed = outcome(
            verified_macos_terminal_owner(root.path(), "session-a", &session, None)
                .map(|(owner, _)| owner.pid),
        );
        assert_eq!(
            observed,
            format!("error {}", read_error(&path, damaged)),
            "{damaged:?}"
        );
    }
}

/// The observation's own answer: the variant, and the binding of a recorded owner.
fn observed(record: &OwnerRecord) -> String {
    match record {
        OwnerRecord::Absent => "absent".to_owned(),
        OwnerRecord::Unreadable(error) => format!("unreadable {error:#}"),
        OwnerRecord::Malformed { path, error } => {
            format!("malformed {}: {error}", path.display())
        }
        OwnerRecord::Recorded(recorded) => {
            format!("recorded {} {:?}", recorded.owner.pid, recorded.binding)
        }
    }
}

// One read policy for every reader, then the binding to the Session whose directory was read.
#[test]
fn the_observation_reads_under_one_policy_and_judges_the_binding() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-a");
    fs::create_dir(&directory).unwrap();
    let path = owner_path(&directory);
    let reader = Reader::open_unchecked(&directory);
    assert_eq!(observed(&observe(&reader)), "absent");
    assert!(absent(&reader));
    assert_eq!(read(&reader).unwrap(), None);
    for (binding, expected) in [
        (Some("session-a"), "recorded 4242 Bound"),
        (None, "recorded 4242 Unbound"),
        (Some("session-b"), "recorded 4242 Foreign"),
    ] {
        Store::open_unchecked(&directory)
            .write_owner(&owner(4242, binding))
            .unwrap();
        assert_eq!(observed(&observe(&reader)), expected, "{binding:?}");
        assert!(!absent(&reader));
        #[cfg(any(target_os = "macos", windows))]
        assert_eq!(required(&reader).unwrap().owner.pid, 4242);
    }
    // A record padded to exactly the bound is read.
    let mut bytes = serde_json::to_vec(&owner(4242, Some("session-a"))).unwrap();
    bytes.resize(LIMIT as usize, b' ');
    fs::write(&path, bytes).unwrap();
    assert_eq!(observed(&observe(&reader)), "recorded 4242 Bound");
    fs::write(&path, b"{").unwrap();
    let malformed = format!(
        "malformed {}: EOF while parsing an object at line 1 column 1",
        path.display()
    );
    assert_eq!(observed(&observe(&reader)), malformed);
    assert!(!absent(&reader));
    assert_eq!(
        format!("{:#}", observe(&reader).decoded().unwrap_err()),
        format!(
            "invalid JSON in {}: EOF while parsing an object at line 1 column 1",
            path.display()
        )
    );
    for &damaged in DAMAGED {
        let _ = fs::remove_dir(&path);
        write_damaged_owner(&directory, &owner(4242, Some("session-a")), damaged);
        let record = observe(&reader);
        assert!(record.unreadable(), "{damaged:?}");
        assert_eq!(
            observed(&record),
            format!("unreadable {}", read_error(&path, damaged)),
            "{damaged:?}"
        );
        // Only a missing record is absent; a dangling link is not.
        assert!(!absent(&reader), "{damaged:?}");
    }
}

// Attestation is made only on request, by the caller's observers, and whatever the binding.
#[test]
fn a_recorded_owner_is_attested_on_request_whatever_session_it_names() {
    for binding in [Some("session-a"), None, Some("session-b")] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-a");
        fs::create_dir(&directory).unwrap();
        Store::open_unchecked(&directory)
            .write_owner(&owner(4242, binding))
            .unwrap();
        let OwnerRecord::Recorded(recorded) = observe(&Reader::open_unchecked(&directory)) else {
            panic!("a written record is recorded");
        };
        let calls = std::cell::Cell::new(0);
        let attestation = attest_owner_with(
            &recorded.owner,
            OwnerObservers {
                pid_alive: &|pid| {
                    assert_eq!(pid, 4242);
                    calls.set(calls.get() + 1);
                    false
                },
                #[cfg(target_os = "macos")]
                birth: &|_| panic!("no birth is recorded"),
                #[cfg(target_os = "macos")]
                control: &|_| panic!("no control identity is recorded"),
                #[cfg(windows)]
                identity: &|_| panic!("no process identity is recorded"),
            },
        );
        assert_eq!(calls.get(), 1, "{binding:?}");
        assert_eq!(
            attestation.process,
            ProcessObservation::Gone(Gone::Exited),
            "{binding:?}"
        );
    }
}
