//! The core Record read policy, one table per record kind: what each lifecycle reader and
//! each query reader of the record makes of a link at the record's path, a dangling link, a
//! directory, bytes that are not UTF-8, and a missing record.
use super::*;
use crate::native::query::observation::SurfaceRecord;
use crate::native::session::turn::JournaledEventState;
use crate::native::tests::seed_close_fixture;
use std::fs;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Link,
    Dangling,
    Directory,
    BadUtf8,
    Missing,
}
use Case::*;

const CASES: [Case; 5] = [Link, Dangling, Directory, BadUtf8, Missing];

/// What a reader made of the entry at the record's path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// The reader returned the record (for a link, through it; for invalid UTF-8, lossily).
    Read,
    /// The reader reported no record.
    Absent,
    /// The reader refused the entry as a non-regular session file.
    Refused,
    /// The reader refused the bytes as invalid UTF-8.
    NotUtf8,
    /// Another failure, such as the OS refusing to read a directory or a required record
    /// being missing.
    Failed,
}
use Seen::*;

fn seen(outcome: Result<Option<()>>) -> (Seen, String) {
    match outcome {
        Ok(Some(())) => (Read, String::new()),
        Ok(None) => (Absent, String::new()),
        Err(error) => {
            let text = format!("{error:#}");
            let seen = if text.contains("refusing non-regular session file") {
                Refused
            } else if text.contains("UTF-8") || text.contains("utf-8") {
                NotUtf8
            } else {
                Failed
            };
            (seen, text)
        }
    }
}

/// A session directory that holds every record a row reads, the row's record valid.
struct Fixture {
    // Keeps the seeded session's temporary root alive.
    _seeded: Box<dyn std::any::Any>,
    directory: PathBuf,
    #[cfg(unix)]
    outside: tempfile::TempDir,
}

fn new_fixture() -> Fixture {
    let seeded = seed_close_fixture(JournaledEventState::Committed);
    Fixture {
        directory: seeded.directory.clone(),
        _seeded: Box::new(seeded),
        #[cfg(unix)]
        outside: tempfile::tempdir().unwrap(),
    }
}

/// The valid bytes of a row's record: fixed, or the record as the seeded session wrote it
/// (its claim token differs from one fixture to the next).
enum Valid {
    Fixed(String),
    Seeded,
}

/// One reader of one record: the record's file name in the session directory, its valid
/// bytes, the text after which a `0xff` byte makes them non-UTF-8 (inside a JSON string, so
/// a lossy reader still decodes them), the reader, and what it makes of each case in
/// [`CASES`] order.
struct Row {
    reader: &'static str,
    file: &'static str,
    valid: Valid,
    corrupt_after: &'static str,
    read: ReadFn,
    expected: [Seen; 5],
}

/// A reader of one record: what it returned, `None` for no record.
type ReadFn = Box<dyn Fn(&Path) -> Result<Option<()>>>;

impl Row {
    fn valid(&self, fixture: &Fixture) -> String {
        match &self.valid {
            Valid::Fixed(text) => text.clone(),
            Valid::Seeded => fs::read_to_string(fixture.directory.join(self.file)).unwrap(),
        }
    }

    fn invalid(&self, fixture: &Fixture) -> Vec<u8> {
        corrupt(&self.valid(fixture), self.corrupt_after)
    }
}

fn install(fixture: &Fixture, row: &Row, case: Case) {
    let path = fixture.directory.join(row.file);
    // Both are taken before the seeded record is removed.
    #[cfg(unix)]
    let valid = row.valid(fixture);
    let invalid = row.invalid(fixture);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
        fs::remove_dir_all(&path).unwrap();
    } else {
        let _ = fs::remove_file(&path);
    }
    match case {
        Link | Dangling => {
            #[cfg(unix)]
            {
                let target = fixture.outside.path().join(row.file.replace('/', "-"));
                if case == Link {
                    fs::write(&target, &valid).unwrap();
                } else {
                    let _ = fs::remove_file(&target);
                }
                std::os::unix::fs::symlink(&target, &path).unwrap();
            }
        }
        Directory => fs::create_dir(&path).unwrap(),
        BadUtf8 => fs::write(&path, &invalid).unwrap(),
        Missing => (),
    }
}

/// Runs every row through every case on a fresh fixture and reports every difference from
/// the expected table at once.
fn check(kind: &str, rows: Vec<Row>) {
    let mut differences = Vec::new();
    for row in &rows {
        // The valid record reads as the record, so a later difference is the case's.
        let fixture = new_fixture();
        fs::write(fixture.directory.join(row.file), row.valid(&fixture)).unwrap();
        let (valid, text) = seen((row.read)(&fixture.directory));
        if valid != Read {
            differences.push(format!("{kind} {} valid: {valid:?} {text}", row.reader));
        }
        for (case, expected) in CASES.into_iter().zip(row.expected) {
            if !cfg!(unix) && matches!(case, Link | Dangling) {
                continue;
            }
            let fixture = new_fixture();
            install(&fixture, row, case);
            let (actual, text) = seen((row.read)(&fixture.directory));
            if actual != expected {
                differences.push(format!(
                    "{kind} {} {case:?}: expected {expected:?}, saw {actual:?} {text}",
                    row.reader
                ));
            }
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

fn reader(directory: &Path) -> Reader {
    Reader::open_unchecked(directory)
}

fn some<T>(value: Result<Option<T>>) -> Result<Option<()>> {
    value.map(|value| value.map(|_| ()))
}

fn always<T>(value: Result<T>) -> Result<Option<()>> {
    value.map(|_| Some(()))
}

fn corrupt(valid: &str, from: &str) -> Vec<u8> {
    assert!(valid.contains(from), "{from} is not in {valid}");
    let mut bytes = Vec::new();
    let (before, after) = valid.split_once(from).unwrap();
    bytes.extend_from_slice(before.as_bytes());
    bytes.extend_from_slice(from.as_bytes());
    bytes.push(0xff);
    bytes.extend_from_slice(after.as_bytes());
    bytes
}

fn row(
    reader: &'static str,
    file: &'static str,
    valid: Valid,
    corrupt_after: &'static str,
    read: ReadFn,
    expected: [Seen; 5],
) -> Row {
    Row {
        reader,
        file,
        valid,
        corrupt_after,
        read,
        expected,
    }
}

fn fixed(text: &str) -> Valid {
    Valid::Fixed(text.to_owned())
}

const STATUS: &str =
    r#"{"state":"working","generation":9,"updated_unix_ms":1,"exit_code":null,"error":"e"}"#;

#[test]
fn status_read_policy() {
    let status =
        |reader, read, expected| row(reader, STATUS_FILE, fixed(STATUS), "\"e", read, expected);
    check(
        "status",
        vec![
            status(
                "Reader::status",
                Box::new(|d| always(reader(d).status())),
                [Refused, Refused, Refused, NotUtf8, Failed],
            ),
            status(
                "Reader::status_if_present",
                Box::new(|d| some(reader(d).status_if_present())),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
            status(
                "Snapshot status",
                Box::new(|d| always(reader(d).snapshot())),
                [Refused, Refused, Refused, NotUtf8, Failed],
            ),
        ],
    );
}

#[test]
fn closed_read_policy() {
    check(
        "closed",
        vec![row(
            "Reader::closed_if_present",
            CLOSED_STATUS_FILE,
            fixed(&STATUS.replace("working", "closed")),
            "\"e",
            Box::new(|d| some(reader(d).closed_if_present())),
            [Refused, Refused, Refused, NotUtf8, Absent],
        )],
    );
}

#[test]
fn manifest_read_policy() {
    let manifest = |reader, read| {
        row(
            reader,
            MANIFEST_FILE,
            Valid::Seeded,
            "\"title\": \"review",
            read,
            [Refused, Refused, Refused, NotUtf8, Failed],
        )
    };
    check(
        "manifest",
        vec![
            manifest(
                "Reader::manifest",
                Box::new(|d| always(reader(d).manifest())),
            ),
            manifest(
                "Snapshot manifest",
                Box::new(|d| always(reader(d).snapshot())),
            ),
        ],
    );
}

/// The fixture's own Claim token, from its receipt: the record under test may be replaced.
fn receipt_token(directory: &Path) -> String {
    fs::read_dir(directory.join(REQUESTS_DIRECTORY))
        .unwrap()
        .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
        .find_map(|name| name.strip_suffix(".json").map(str::to_owned))
        .unwrap()
}

#[test]
fn turn_claim_read_policy() {
    // The token is digits and dashes; the corruption follows its first digit.
    let claim =
        |reader, read, expected| row(reader, TURN_CLAIM_FILE, Valid::Seeded, "", read, expected);
    check(
        "claim",
        vec![
            claim(
                "turn::current_claim_token",
                Box::new(|d| some(turn::current_claim_token(&reader(d)))),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
            claim(
                "turn::published (the wait barrier)",
                // Unpublished while the receipt's Claim is installed.
                Box::new(|d| {
                    turn::published(&reader(d), Some(&receipt_token(d)))
                        .map(|published| (!published).then_some(()))
                }),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
            claim(
                "Snapshot claim",
                Box::new(|d| some(reader(d).snapshot().map(|snapshot| snapshot.claim))),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
        ],
    );
}

#[test]
fn completion_read_policy() {
    let journal = |reader, read, expected| {
        row(
            reader,
            TURN_COMPLETION_FILE,
            Valid::Seeded,
            "\"late",
            read,
            expected,
        )
    };
    check(
        "completion",
        vec![
            journal(
                "turn::recover_pending_completion",
                Box::new(|d| turn::recover_pending_completion(d).map(|done| done.then_some(()))),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
            journal(
                "Snapshot journal",
                Box::new(|d| some(reader(d).snapshot().map(|snapshot| snapshot.pending))),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
        ],
    );
}

#[test]
fn terminal_read_policy() {
    let handle = |reader, read, expected| {
        row(
            reader,
            TERMINAL_HANDLE_FILE,
            Valid::Seeded,
            "\"42",
            read,
            expected,
        )
    };
    check(
        "terminal",
        vec![
            handle(
                "Reader::terminal",
                Box::new(|d| always(reader(d).terminal())),
                [Refused, Refused, Refused, NotUtf8, Failed],
            ),
            handle(
                "launch::pending::bound_surface",
                Box::new(|d| {
                    some(launch::pending::bound_surface(
                        &reader(d),
                        "session-close123",
                        || "invalid",
                    ))
                }),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
            handle(
                "SurfaceRecord::read (active)",
                Box::new(|d| match SurfaceRecord::read(&reader(d)) {
                    SurfaceRecord::Active(result) => some(result),
                    _ => panic!("no tombstone or closing handle in this fixture"),
                }),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
        ],
    );
}

fn terminal_handle() -> String {
    serde_json::to_string(&crate::native::session::test_support::lifecycle::close_test_terminal())
        .unwrap()
}

#[test]
fn terminal_closing_read_policy() {
    check(
        "terminal.closing",
        vec![row(
            "SurfaceRecord::read (closing)",
            TERMINAL_CLOSING_FILE,
            Valid::Fixed(terminal_handle()),
            "\"42",
            Box::new(|d| match SurfaceRecord::read(&reader(d)) {
                SurfaceRecord::Closing(result) => always(result),
                SurfaceRecord::Active(_) => Ok(None),
                SurfaceRecord::Closed(_) => panic!("no tombstone in this fixture"),
            }),
            [Refused, Refused, Refused, NotUtf8, Absent],
        )],
    );
}

#[test]
fn terminal_closed_read_policy() {
    let tombstone = |reader, read, expected| {
        row(
            reader,
            TERMINAL_TOMBSTONE_FILE,
            fixed(r#"{"consumed":true,"terminal":null,"reason":"kept"}"#),
            "\"kept",
            read,
            expected,
        )
    };
    check(
        "terminal.closed",
        vec![
            tombstone(
                "Reader::terminal_closed",
                Box::new(|d| always(reader(d).terminal_closed())),
                [Refused, Refused, Refused, NotUtf8, Failed],
            ),
            tombstone(
                "SurfaceRecord::read (closed)",
                Box::new(|d| match SurfaceRecord::read(&reader(d)) {
                    SurfaceRecord::Closed(result) => always(result),
                    _ => Ok(None),
                }),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
        ],
    );
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_close_intent_read_policy() {
    // Both close-intent readers take the record's text through the same read.
    check(
        "terminal.close-intent",
        vec![row(
            "close intent text",
            TERMINAL_CLOSE_INTENT_FILE,
            fixed(r#"{"other":"x"}"#),
            "\"x",
            Box::new(|d| some(reader(d).record(CoreRecord::TerminalCloseIntent).text())),
            [Refused, Refused, Refused, NotUtf8, Absent],
        )],
    );
}

#[test]
fn hold_read_policy() {
    let hold = |reader, read| {
        row(
            reader,
            HOLD_FILE,
            fixed(r#"{"held":true,"created_unix_ms":1,"note":"n"}"#),
            "\"n",
            read,
            [Refused, Refused, Refused, Failed, Absent],
        )
    };
    check(
        "hold",
        vec![
            hold(
                "hold::read",
                Box::new(|d| hold::read(&reader(d)).map(|held| held.then_some(()))),
            ),
            hold(
                "hold::observe",
                Box::new(|d| hold::observe(&reader(d), true).map(|held| held.then_some(()))),
            ),
        ],
    );
}

#[test]
fn cancel_read_policy() {
    let cancel = |reader, read| {
        row(
            reader,
            "cancel.json",
            fixed(
                r#"{"schema":1,"request_id":"request-1","claim_token":"1-2-3","created_unix_ms":1,"extra":"x"}"#,
            ),
            "\"extra\":\"x",
            read,
            [Refused, Refused, Refused, NotUtf8, Absent],
        )
    };
    check(
        "cancel",
        vec![
            cancel("cancel::read", Box::new(|d| some(cancel::read(&reader(d))))),
            cancel(
                "cancel::observe",
                Box::new(|d| some(cancel::observe(&reader(d), true))),
            ),
        ],
    );
}

#[test]
fn reopen_marker_read_policy() {
    check(
        "reopen.marker",
        vec![row(
            "Reader::reopen_marker_if_present",
            REOPEN_MARKER_FILE,
            fixed(r#"{"schema":1,"claim":"c","provider_session_id":"p","created_unix_ms":1}"#),
            "\"claim\":\"c",
            Box::new(|d| some(reader(d).reopen_marker_if_present())),
            [Refused, Refused, Refused, NotUtf8, Absent],
        )],
    );
}

#[test]
fn provider_process_read_policy() {
    let process = |reader, read, expected| {
        row(
            reader,
            PROVIDER_PROCESS_FILE,
            fixed(
                r#"{"schema":1,"managed_session_id":"session-fault","pid":42,"spawned_unix_ms":1}"#,
            ),
            "\"session-fault",
            read,
            expected,
        )
    };
    check(
        "provider-process",
        vec![
            process(
                "Reader::provider_process",
                Box::new(|d| always(reader(d).provider_process())),
                [Refused, Refused, Refused, NotUtf8, Failed],
            ),
            process(
                "Reader::provider_process_if_present",
                Box::new(|d| some(reader(d).provider_process_if_present())),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
        ],
    );
}

#[test]
fn initial_prompt_read_policy() {
    check(
        "initial-prompt",
        vec![row(
            "Reader::initial_prompt",
            INITIAL_PROMPT_FILE,
            fixed("hello"),
            "he",
            Box::new(|d| always(reader(d).initial_prompt())),
            [Refused, Refused, Refused, NotUtf8, Failed],
        )],
    );
}

#[test]
fn event_read_policy() {
    // An event no journal or receipt names: published, and read by every event reader.
    let event = |reader, read, expected| {
        row(
            reader,
            "events/orphan.json",
            fixed(r#"{"provider":"codex","message":"orphan","error":null}"#),
            "\"orphan",
            read,
            expected,
        )
    };
    check(
        "event",
        vec![
            event(
                "lifecycle event read (turn, reopen, Codex correlation)",
                Box::new(|d| {
                    always(
                        CoreRecordReader::at(&d.join("events/orphan.json"))
                            .json_with(record::event),
                    )
                }),
                [Refused, Refused, Refused, NotUtf8, Failed],
            ),
            event(
                "Snapshot::event",
                Box::new(|d| {
                    let reader = reader(d);
                    some(reader.snapshot()?.event(&reader, "orphan.json"))
                }),
                [Refused, Refused, Refused, NotUtf8, Absent],
            ),
            event(
                "Reader::event_strict",
                Box::new(|d| {
                    always(
                        reader(d)
                            .event_strict("orphan.json")
                            .map_err(anyhow::Error::msg),
                    )
                }),
                [Refused, Refused, Refused, NotUtf8, Failed],
            ),
            // The bytes only: the search charges them, then decodes them strictly.
            event(
                "search budgeted event read",
                Box::new(
                    |d| match reader(d).event("orphan.json").event_within_budget(1 << 20) {
                        Ok(text) => Ok(text.map(|_| ())),
                        Err(Ok(size)) => anyhow::bail!("over budget: {size}"),
                        Err(Err(error)) => Err(error),
                    },
                ),
                [Refused, Refused, Refused, Read, Absent],
            ),
        ],
    );
}

#[test]
fn launch_and_receipt_read_policy() {
    // The receipt's file name is the fixture's own Claim token, so its row reads the one
    // receipt file it finds.
    check(
        "launch and receipt",
        vec![row(
            "launch::read",
            LAUNCH_FILE,
            fixed(
                r#"{"schema":1,"claim_token":"1-2-3","deadline_unix_ms":11,"phase":"pending","note":"n"}"#,
            ),
            "\"n",
            Box::new(|d| some(launch::read(&reader(d)))),
            [Refused, Refused, Refused, NotUtf8, Absent],
        )],
    );
    let fixture = new_fixture();
    let token = receipt_token(&fixture.directory);
    let receipt = fixture
        .directory
        .join(REQUESTS_DIRECTORY)
        .join(format!("{token}.json"));
    let valid = fs::read_to_string(&receipt).unwrap();
    let read = || requests::for_claim(&reader(&fixture.directory), &token);
    fs::write(&receipt, corrupt(&valid, "\"request-")).unwrap();
    assert!(format!("{:#}", read().unwrap_err()).contains("invalid UTF-8"));
    fs::remove_file(&receipt).unwrap();
    assert!(read().unwrap().is_none());
    fs::create_dir(&receipt).unwrap();
    assert!(format!("{:#}", read().unwrap_err()).contains("refusing non-regular session file"));
}

// Cancel admission reads the completion journal to decide whether the request's result is
// already published.
#[test]
fn cancel_admission_over_a_journal_that_is_not_utf8() {
    let fixture = new_fixture();
    let reader = reader(&fixture.directory);
    let admit = || {
        cancel::admission(&reader, "session-fault", || {
            Ok(crate::native::provider::CancelSupport::Supported)
        })
        .map(|admitted| admitted.request_id)
        .map_err(|error| format!("{error:#}"))
    };
    assert!(
        admit()
            .unwrap_err()
            .contains("already has a published result")
    );
    let journal = fixture.directory.join(TURN_COMPLETION_FILE);
    let valid = fs::read_to_string(&journal).unwrap();
    fs::write(&journal, corrupt(&valid, "\"late")).unwrap();
    // A journal that is not UTF-8 is unreadable: admission fails on it.
    let error = admit().unwrap_err();
    assert!(
        error.contains("stream did not contain valid UTF-8"),
        "{error}"
    );
}

// The presence checks of the records lifecycle steps look for before acting take the read
// policy's answer for every entry but a missing one: a regular file is present whatever its
// bytes, and anything else is refused.
#[test]
fn presence_read_policy() {
    let rows = [
        (TURN_CLAIM_FILE, Valid::Seeded, ""),
        (TURN_COMPLETION_FILE, Valid::Seeded, "\"late"),
        (TERMINAL_HANDLE_FILE, Valid::Seeded, "\"42"),
        (
            TERMINAL_CLOSING_FILE,
            Valid::Fixed(terminal_handle()),
            "\"42",
        ),
        (
            TERMINAL_TOMBSTONE_FILE,
            fixed(r#"{"consumed":true,"terminal":null,"reason":"kept"}"#),
            "\"kept",
        ),
        (
            LEGACY_RESUME_PENDING_FILE,
            fixed(r#"{"legacy":"x"}"#),
            "\"x",
        ),
        (
            LEGACY_RESUME_RUNNING_FILE,
            fixed(r#"{"legacy":"x"}"#),
            "\"x",
        ),
    ]
    .into_iter()
    .map(|(file, valid, corrupt_after)| {
        row(
            "CoreRecordReader::present",
            file,
            valid,
            corrupt_after,
            Box::new(move |d| {
                CoreRecordReader::at(&d.join(file))
                    .present()
                    .map(|present| present.then_some(()))
            }),
            [Refused, Refused, Refused, Read, Absent],
        )
    })
    .collect();
    check("presence", rows);
}

#[test]
fn launch_host_read_policy() {
    use launch::host::HostObservation;
    // A schema this build does not read still shows the record was read and decoded.
    check(
        "launch-host",
        vec![row(
            "launch::host::observe",
            CoreRecord::LaunchHost.name(),
            fixed(r#"{"schema":2,"note":"n"}"#),
            "\"n",
            Box::new(|d| match launch::host::observe(&reader(d), None) {
                HostObservation::NotRecorded => Ok(None),
                HostObservation::Unreadable(error) => Err(anyhow::anyhow!(error)),
                _ => Ok(Some(())),
            }),
            [Refused, Refused, Refused, NotUtf8, Absent],
        )],
    );
}
