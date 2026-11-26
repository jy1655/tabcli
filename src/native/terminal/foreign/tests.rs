// The judgment needs an OS whose terminals it knows.
#![cfg(any(target_os = "macos", target_os = "linux", windows))]
use super::*;
use crate::native::failure_kind::{FailureKind, classify};
use crate::native::session::record::{terminal, terminal_stated};
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

// One session directory with the given Records (a string is written as it is), judged as the
// close transaction judges it: each Record read once.
fn judge_records(records: &[(&str, Value)]) -> Option<Elsewhere> {
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
    of_records(&Reader::open_unchecked(&directory))
}

// Each positive statement in a Record that is this session's own: the bound active handle's
// stated kind or (Herdr only) host birth, a strict marker's kind, and a bound Owner record's
// identity.
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
                ("native-session.json", pid_only),
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
        assert_eq!(judge_records(&records), Some(expected), "{name}");
    }
}

// Nothing else is evidence, and such a session is this OS's: a field that does not belong to
// the handle's kind, a Record bound to another session or to none, a claimed handle, a marker
// that does not decode, repeats `consumed` or `terminal`, or does not say `consumed: true`, a
// missing or null kind, and a Record its reader refuses.
#[test]
fn nothing_but_a_positive_statement_in_the_sessions_own_records_is_evidence() {
    let foreign = foreign_kind();
    let native = native_kind();
    let other = other_os();
    let live_pid = json!({"pid": std::process::id(), "managed_session_id": ID});
    let mut native_with_herdr_field = handle(native, Some(ID));
    native_with_herdr_field["herdr_core"] = herdr_core(other);
    let mut owner_of_another_session = owner_shape(other);
    owner_of_another_session["managed_session_id"] = json!("session-other");
    let legacy_owner = owner_shape(other);
    let mut empty_id = handle(foreign, Some(ID));
    empty_id["session_id"] = json!("");
    let foreign_name = foreign.as_str();
    let native_name = native.as_str();
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
                ("terminal.json", handle(native, Some(ID))),
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
            "a claimed Herdr handle born on another OS",
            vec![(
                "terminal.closing.json",
                json!({"terminal": "herdr", "session_id": "t7", "managed_session_id": ID,
                    "herdr_core": herdr_core(other)}),
            )],
        ),
        (
            "a handle of such a kind that repeats its kind",
            vec![(
                "terminal.json",
                Value::String(format!(
                    r#"{{"terminal":"{foreign_name}","terminal":"{foreign_name}","session_id":"s","managed_session_id":"{ID}"}}"#
                )),
            )],
        ),
        (
            "a marker that says consumed: false, beside a live PID",
            vec![
                (
                    "terminal.closed.json",
                    json!({"consumed": false, "terminal": foreign}),
                ),
                ("native-session.json", live_pid.clone()),
            ],
        ),
        (
            "a marker whose consumed field is repeated, beside a live PID",
            vec![
                (
                    "terminal.closed.json",
                    Value::String(format!(
                        r#"{{"consumed":false,"consumed":true,"terminal":"{foreign_name}"}}"#
                    )),
                ),
                ("native-session.json", live_pid.clone()),
            ],
        ),
        (
            "a marker whose terminal field is repeated, beside a live PID",
            vec![
                (
                    "terminal.closed.json",
                    Value::String(format!(
                        r#"{{"consumed":true,"terminal":"{native_name}","terminal":"{foreign_name}"}}"#
                    )),
                ),
                ("native-session.json", live_pid),
            ],
        ),
        (
            "a marker with no consumed fact",
            vec![("terminal.closed.json", json!({"terminal": foreign}))],
        ),
        (
            "a marker whose consumed fact is not a boolean",
            vec![(
                "terminal.closed.json",
                json!({"consumed": "true", "terminal": foreign}),
            )],
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
                json!({"consumed": true, "terminal": native}),
            )],
        ),
        (
            "an active handle its reader refuses (not UTF-8)",
            vec![("terminal.json", Value::String("\u{fffd}".into()))],
        ),
    ];
    for (name, records) in rejected {
        assert_eq!(judge_records(&records), None, "{name}");
    }
}

// The judgment needs a kind the handle record itself states. The reader supplies iTerm2 for
// a handle that omits its kind (a legacy record); that default is never evidence, on any OS,
// while a handle that states iTerm2 is evidence where iTerm2 does not run.
#[test]
fn a_kind_the_handle_record_does_not_state_is_never_evidence() {
    let omitted =
        terminal_stated(&json!({"session_id": "s", "managed_session_id": ID}).to_string()).unwrap();
    let stated = terminal_stated(
        &json!({"terminal": "iterm2", "session_id": "s", "managed_session_id": ID}).to_string(),
    )
    .unwrap();
    assert_eq!(omitted.handle.kind, TerminalKind::Iterm2);
    assert!(!omitted.kind_stated && stated.kind_stated);
    for os in [OsTerminals::MACOS, OsTerminals::LINUX, OsTerminals::WINDOWS] {
        let judged = |handle| {
            judge_on(
                os,
                ID,
                Records {
                    active: Some(handle),
                    ..Records::default()
                },
            )
        };
        assert_eq!(judged(&omitted), None, "an omitted kind on {}", os.os);
        assert_eq!(
            judged(&stated),
            (!os.runs(TerminalKind::Iterm2)).then_some(Elsewhere::Kind(TerminalKind::Iterm2)),
            "a stated iTerm2 on {}",
            os.os
        );
    }
}

// The handle reader that keeps whether the kind was stated is the handle reader: the same
// decode over the same text, which accepts, refuses and returns exactly what the ordinary one
// does, a deeply nested field it does not know included, and adds only whether the record's
// own fields named `terminal`.
#[test]
fn the_stated_handle_reader_decodes_exactly_as_the_handle_reader() {
    let deep = format!(
        r#"{{"terminal":"tmux","session_id":"s","x":{}1{}}}"#,
        "[".repeat(200),
        "]".repeat(200)
    );
    let cases: Vec<(String, Option<bool>)> = vec![
        (
            json!({"terminal": "tmux", "session_id": "s"}).to_string(),
            Some(true),
        ),
        (json!({"session_id": "s"}).to_string(), Some(false)),
        (json!({"iterm_session_id": "s"}).to_string(), Some(false)),
        (
            r#"{"termin\u0061l":"warp","session_id":"s"}"#.to_owned(),
            Some(true),
        ),
        (
            json!({"session_id": "s", "x": {"terminal": "tmux"}}).to_string(),
            Some(false),
        ),
        (
            json!({"terminal": "herdr", "session_id": "t7", "herdr_core": {"socket": "/s",
                "core_id": "c", "created_ms": 1, "host_pid": 4, "host_creation_time": 1}})
            .to_string(),
            Some(true),
        ),
        (deep, None),
        (
            r#"{"terminal":"tmux","terminal":"tmux","session_id":"s"}"#.to_owned(),
            None,
        ),
        (
            json!({"terminal": null, "session_id": "s"}).to_string(),
            None,
        ),
        (
            json!({"terminal": "kitty", "session_id": "s"}).to_string(),
            None,
        ),
        (json!(["tmux", "s"]).to_string(), None),
        (
            r#"{"terminal":"tmux","session_id":"s"} trailing"#.to_owned(),
            None,
        ),
        ("5".to_owned(), None),
    ];
    for (text, stated) in cases {
        let ordinary = terminal(&text);
        let kept = terminal_stated(&text);
        match (&ordinary, &kept) {
            (Ok(handle), Ok(read)) => assert_eq!(*handle, read.handle, "{text}"),
            (Err(ordinary), Err(kept)) => {
                assert_eq!(ordinary.to_string(), kept.to_string(), "{text}")
            }
            _ => panic!("{text}: {ordinary:?} against {kept:?}"),
        }
        if let Some(stated) = stated {
            assert_eq!(kept.unwrap().kind_stated, stated, "{text}");
        }
    }
}

// A marker counts once and only when it says `consumed: true`: a repeated `consumed` or
// `terminal`, or a `consumed` that is not `true`, is no completed consumption; its kind is
// optional (absent, null or one this version does not know is none).
#[test]
fn a_marker_states_a_completed_consumption_only_strictly() {
    use crate::native::session::record::{Consumption, consumption};
    let kind = |kind| Some(Consumption { kind });
    for (text, expected) in [
        (
            r#"{"consumed":true,"terminal":"tmux"}"#,
            kind(Some(TerminalKind::Tmux)),
        ),
        (r#"{"consumed":true}"#, kind(None)),
        (r#"{"consumed":true,"terminal":null}"#, kind(None)),
        (r#"{"consumed":true,"terminal":"kitty"}"#, kind(None)),
        (
            r#"{"consumed":true,"terminal":"warp","reason":"gone","x":[1]}"#,
            kind(Some(TerminalKind::Warp)),
        ),
        (r#"{"consumed":false,"terminal":"tmux"}"#, None),
        (
            r#"{"consumed":false,"consumed":true,"terminal":"tmux"}"#,
            None,
        ),
        (
            r#"{"consumed":true,"consumed":true,"terminal":"tmux"}"#,
            None,
        ),
        (
            r#"{"consumed":true,"terminal":"tmux","terminal":"warp"}"#,
            None,
        ),
        (r#"{"consumed":"true","terminal":"tmux"}"#, None),
        (r#"{"terminal":"tmux"}"#, None),
        (r#"{"consumed":true,"terminal":5}"#, None),
        ("{broken", None),
        ("[true]", None),
        (" \n[true, \"tmux\"]", None),
        (" \n{\"consumed\":true}", kind(None)),
    ] {
        assert_eq!(consumption(text.as_bytes()), expected, "{text}");
    }
    assert_eq!(consumption(b"\xff"), None);
}

// The refusal names what the Records name and the directory of the records, and no terminal
// to use instead; a Herdr host birth counts for a Herdr handle only.
#[test]
fn each_statement_gives_one_refusal() {
    let os = here().os;
    for kind in TerminalKind::ALL {
        let judged = of_handle_on(here(), &TerminalSession::new(kind, "surface".into()));
        if kind.supported_on_this_platform() {
            assert_eq!(judged, None, "{kind:?}");
        } else {
            assert_eq!(judged, Some(Elsewhere::Kind(kind)), "{kind:?}");
            assert_eq!(
                judged.unwrap().head(),
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
        of_owner_on(here(), &mixed),
        Some(Elsewhere::Recorded("more than one operating system"))
    );
    assert_eq!(
        of_owner_on(here(), &serde_json::from_value(owner_shape(os)).unwrap()),
        None
    );
}
