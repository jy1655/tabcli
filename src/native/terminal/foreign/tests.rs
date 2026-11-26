// The judgment needs an OS whose terminals it knows.
#![cfg(any(target_os = "macos", target_os = "linux", windows))]
use super::*;
use crate::native::failure_kind::{FailureKind, classify};
use serde_json::{Value, json};

const ID: &str = "session-copy";

fn here() -> OsTerminals {
    OsTerminals::CURRENT.unwrap()
}

fn other_os() -> &'static str {
    ["macOS", "Linux", "Windows"]
        .into_iter()
        .find(|os| *os != here().os)
        .unwrap()
}

// A kind this OS does not run, and one it runs other than Herdr.
fn foreign_kind() -> TerminalKind {
    TerminalKind::ALL
        .into_iter()
        .find(|kind| !kind.supported_on_this_platform())
        .unwrap()
}

fn native_kind() -> TerminalKind {
    TerminalKind::ALL
        .into_iter()
        .find(|kind| kind.supported_on_this_platform() && *kind != TerminalKind::Herdr)
        .unwrap()
}

// The Owner record each OS's writer records, as JSON.
fn owner_shape(os: &str) -> Value {
    match os {
        "macOS" => {
            json!({"pid": 4242, "process_start_seconds": 1, "process_start_microseconds": 2})
        }
        "Linux" => json!({"pid": 4242, "linux_birth": {
            "boot_id": "00112233-4455-6677-8899-aabbccddeeff", "starttime_ticks": 7}}),
        _ => json!({"pid": 4242, "windows_process_identity": {
            "creation_time": 1, "executable_path": r"C:\Program Files\PowerShell\7\pwsh.exe"}}),
    }
}

fn herdr_core(os: &str) -> Value {
    let mut core = json!({"socket": "/tmp/core.sock", "core_id": "core-a", "created_ms": 1,
        "host_pid": 4});
    let birth = match os {
        "macOS" => json!({"host_start_seconds": 1, "host_start_microseconds": 2}),
        "Linux" => json!({"linux_birth": {
            "boot_id": "00112233-4455-6677-8899-aabbccddeeff", "starttime_ticks": 7}}),
        _ => json!({"host_creation_time": 1}),
    };
    for (key, value) in birth.as_object().unwrap() {
        core[key] = value.clone();
    }
    core
}

fn handle(kind: TerminalKind, binding: Option<&str>) -> Value {
    let mut handle = json!({"terminal": kind, "session_id": "surface"});
    if let Some(binding) = binding {
        handle["managed_session_id"] = json!(binding);
    }
    handle
}

// One session directory with the given Records, judged by `of_records`, and the verdict the
// Owner record's one observation carries.
fn judge(records: &[(&str, Value)]) -> (Option<Elsewhere>, Option<Elsewhere>) {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join(ID);
    std::fs::create_dir(&directory).unwrap();
    for (name, value) in records {
        let path = directory.join(name);
        match value {
            Value::String(raw) => std::fs::write(path, raw).unwrap(),
            value => std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap(),
        }
    }
    let reader = Reader::open_unchecked(&directory);
    let observed = match session::owner::observe(&reader) {
        session::owner::OwnerRecord::Recorded(recorded) => recorded.owner.recorded_elsewhere,
        _ => None,
    };
    (of_records(&reader), observed)
}

// Each positive statement in a Record that is this session's own: the bound active handle's
// kind or (Herdr only) host birth, a valid consumption marker's kind, and a bound Owner
// record's identity. The Owner record's observation carries the same verdict.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn a_session_belongs_to_another_os_only_on_its_own_records_positive_statement() {
    let foreign = foreign_kind();
    let other = other_os();
    let pid_only = json!({"pid": std::process::id(), "managed_session_id": ID});
    let mut bound_owner = owner_shape(other);
    bound_owner["managed_session_id"] = json!(ID);
    let mut herdr = handle(TerminalKind::Herdr, Some(ID));
    herdr["herdr_core"] = herdr_core(other);
    type Row = (&'static str, Vec<(&'static str, Value)>, Elsewhere);
    let accepted: [Row; 5] = [
        (
            "a bound active handle of a kind this OS does not run",
            vec![
                ("terminal.json", handle(foreign, Some(ID))),
                ("native-session.json", pid_only.clone()),
            ],
            Elsewhere::Kind(foreign),
        ),
        (
            "a bound Herdr handle born on another OS",
            vec![("terminal.json", herdr)],
            Elsewhere::Recorded(other),
        ),
        (
            "a consumption marker that says consumed and names such a kind",
            vec![
                (
                    "terminal.closed.json",
                    json!({"consumed": true, "terminal": foreign}),
                ),
                ("native-session.json", pid_only.clone()),
            ],
            Elsewhere::Kind(foreign),
        ),
        (
            "an Owner record bound to the session with another OS's identity",
            vec![("native-session.json", bound_owner.clone())],
            Elsewhere::Recorded(other),
        ),
        (
            "the same Owner record beside an unbound handle",
            vec![
                ("terminal.json", handle(native_kind(), None)),
                ("native-session.json", bound_owner),
            ],
            Elsewhere::Recorded(other),
        ),
    ];
    for (name, records, expected) in accepted {
        let (judged, observed) = judge(&records);
        assert_eq!(judged, Some(expected), "{name}");
        if records
            .iter()
            .any(|(record, _)| *record == "native-session.json")
        {
            assert_eq!(observed, Some(expected), "{name}");
        }
    }
}

// Nothing else is evidence, and such a session is this OS's: a field that does not belong to
// the handle's kind, a Record bound to another session or to none, a marker that does not
// decode or says `consumed: false`, a claimed handle, a missing or null kind. The reviewer's
// fixtures: a bound native handle carrying another OS's Herdr host field beside a live PID, a
// Windows Owner record bound to another session, and `consumed: false` markers.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn nothing_but_a_positive_statement_in_the_sessions_own_records_is_evidence() {
    let foreign = foreign_kind();
    let other = other_os();
    let live_pid = json!({"pid": std::process::id(), "managed_session_id": ID});
    let mut native_with_herdr_field = handle(native_kind(), Some(ID));
    native_with_herdr_field["herdr_core"] = herdr_core(other);
    let mut owner_of_another_session = owner_shape(other);
    owner_of_another_session["managed_session_id"] = json!("session-other");
    let mut legacy_owner = owner_shape(other);
    legacy_owner
        .as_object_mut()
        .unwrap()
        .remove("managed_session_id");
    let mut empty_id = handle(foreign, Some(ID));
    empty_id["session_id"] = json!("");
    let rejected: Vec<(&str, Vec<(&str, Value)>)> = vec![
        ("no records", vec![]),
        (
            "a bound native handle with another OS's Herdr field, beside a live PID",
            vec![
                ("terminal.json", native_with_herdr_field),
                ("native-session.json", live_pid.clone()),
            ],
        ),
        (
            "another OS's Owner record bound to another session",
            vec![
                ("terminal.json", handle(native_kind(), Some(ID))),
                ("native-session.json", owner_of_another_session),
            ],
        ),
        (
            "another OS's Owner record bound to none",
            vec![("native-session.json", legacy_owner)],
        ),
        (
            "a handle of such a kind bound to another session",
            vec![
                ("terminal.json", handle(foreign, Some("session-other"))),
                ("native-session.json", live_pid.clone()),
            ],
        ),
        (
            "a handle of such a kind bound to none",
            vec![("terminal.json", handle(foreign, None))],
        ),
        (
            "a bound handle of such a kind with no surface id",
            vec![("terminal.json", empty_id)],
        ),
        (
            "a claimed handle of such a kind",
            vec![("terminal.closing.json", handle(foreign, Some(ID)))],
        ),
        (
            "a marker that says consumed: false, beside a live PID",
            vec![
                (
                    "terminal.closed.json",
                    json!({"consumed": false, "terminal": foreign}),
                ),
                ("native-session.json", live_pid),
            ],
        ),
        (
            "a marker with no consumed fact",
            vec![("terminal.closed.json", json!({"terminal": foreign}))],
        ),
        (
            "a marker that does not decode",
            vec![("terminal.closed.json", Value::String("{broken".into()))],
        ),
        (
            "a marker with a null kind",
            vec![(
                "terminal.closed.json",
                json!({"consumed": true, "terminal": null}),
            )],
        ),
        (
            "a marker of a kind this OS runs",
            vec![(
                "terminal.closed.json",
                json!({"consumed": true, "terminal": native_kind()}),
            )],
        ),
    ];
    for (name, records) in rejected {
        assert_eq!(judge(&records), (None, None), "{name}");
    }
}

// The refusal names what the Records name and the directory of the records, and no terminal
// to use instead; a Herdr host birth counts for a Herdr handle only.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn each_statement_gives_one_refusal() {
    let os = here().os;
    for kind in TerminalKind::ALL {
        let judged = of_handle(&TerminalSession::new(kind, "surface".into()));
        if kind.supported_on_this_platform() {
            assert_eq!(judged, None, "{kind:?}");
        } else {
            assert_eq!(judged, Some(Elsewhere::Kind(kind)), "{kind:?}");
            assert_eq!(
                judged.unwrap().reason(),
                format!("{} is not available on {os}", kind.display_name())
            );
        }
    }
    let error = Elsewhere::Recorded(other_os()).refusal(Path::new("records"));
    assert_eq!(
        format!("{error:#}"),
        format!(
            "a session recorded on {} is not available on {os}: TAB on this machine cannot \
             judge or end this session; once its records show it ended, an explicit close \
             settles it (its records are in records)",
            other_os()
        )
    );
    assert_eq!(classify(&error), Some(FailureKind::Unsupported));
    let mut mixed: NativeSessionOwner = serde_json::from_value(owner_shape("Linux")).unwrap();
    mixed.windows_process_identity =
        serde_json::from_value::<NativeSessionOwner>(owner_shape("Windows"))
            .unwrap()
            .windows_process_identity;
    assert_eq!(
        of_owner(&mixed),
        Some(Elsewhere::Recorded("more than one operating system"))
    );
    assert_eq!(
        of_owner(&serde_json::from_value(owner_shape(os)).unwrap()),
        None
    );
}
