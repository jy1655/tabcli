//! Close and interrupted-close convergence over core records the read policy refuses or
//! cannot decode. Close settles a completion journal from its bytes, and compares a
//! journaled event's bytes, so invalid UTF-8 never keeps a session from closing; a link at
//! a core record's path is refused wherever Close reads or checks for that record.
use super::*;
use crate::native::session::names::*;
use crate::native::session::primitives::read_json;
use crate::native::tests::seed_close_fixture;
use std::path::PathBuf;

struct Seeded {
    // Keeps the seeded session's temporary root alive.
    _fixture: Box<dyn std::any::Any>,
    directory: PathBuf,
}

fn seeded(journaled_event: JournaledEventState) -> Seeded {
    let fixture = seed_close_fixture(journaled_event);
    Seeded {
        directory: fixture.directory.clone(),
        _fixture: Box::new(fixture),
    }
}

/// Puts a `0xff` byte after the first `after` in the record, inside a JSON string.
fn corrupt_in_place(path: &Path, after: &str) {
    let text = fs::read_to_string(path).unwrap();
    let (before, rest) = text.split_once(after).unwrap();
    let mut bytes = before.as_bytes().to_vec();
    bytes.extend_from_slice(after.as_bytes());
    bytes.push(0xff);
    bytes.extend_from_slice(rest.as_bytes());
    fs::write(path, bytes).unwrap();
}

/// An explicit close through the whole procedure; the adapter's calls are counted.
fn close(directory: &Path) -> Result<usize> {
    let mut calls = 0;
    session::close::close(
        &Store::open_unchecked(directory),
        Some("closed by the maintainer".to_owned()),
        budget::close_deadline(),
        |_| {
            calls += 1;
            Ok(terminal::CloseOutcome::Closed.into())
        },
    )
    .map(|_| calls)
}

fn status(directory: &Path) -> SessionStatus {
    read_json(&directory.join(STATUS_FILE)).unwrap()
}

fn present(directory: &Path, name: &str) -> bool {
    fs::symlink_metadata(directory.join(name)).is_ok()
}

fn assert_closed_and_settled(directory: &Path) {
    let status = status(directory);
    assert_eq!(status.state.as_str(), "closed");
    let tombstone: SessionStatus = read_json(&directory.join(CLOSED_STATUS_FILE)).unwrap();
    assert_eq!(tombstone.generation, status.generation);
    for name in [
        TURN_CLAIM_FILE,
        TURN_COMPLETION_FILE,
        LEGACY_RESUME_PENDING_FILE,
        TERMINAL_HANDLE_FILE,
        TERMINAL_CLOSING_FILE,
    ] {
        assert!(!present(directory, name), "{name} remains");
    }
}

// A journal that is not UTF-8 is not evidence: Close settles without it, keeps the event the
// journal named, and a repeated Close changes nothing.
#[test]
fn close_settles_a_journal_that_is_not_utf8_and_repeats() {
    let seeded = seeded(JournaledEventState::Committed);
    let directory = seeded.directory.as_path();
    let journal: serde_json::Value = read_json(&directory.join(TURN_COMPLETION_FILE)).unwrap();
    let event = directory
        .join(EVENTS_DIRECTORY)
        .join(journal["event_file"].as_str().unwrap());
    let event_bytes = fs::read(&event).unwrap();
    corrupt_in_place(&directory.join(TURN_COMPLETION_FILE), "\"late");

    assert_eq!(close(directory).unwrap(), 1);
    assert_closed_and_settled(directory);
    let closed = status(directory);
    assert!(
        closed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("invalid pending native turn completion")),
        "{:?}",
        closed.error
    );
    assert_eq!(fs::read(&event).unwrap(), event_bytes);

    assert_eq!(close(directory).unwrap(), 0);
    assert_closed_and_settled(directory);
    assert_eq!(status(directory).generation, closed.generation);
    assert_eq!(fs::read(&event).unwrap(), event_bytes);
}

// A valid journal naming an event that is not UTF-8: the publication predicate compares the
// event's bytes with the journal's, finds them different, and Close sets the event aside.
#[test]
fn close_sets_aside_an_event_that_is_not_utf8_under_a_valid_journal_and_repeats() {
    let seeded = seeded(JournaledEventState::Committed);
    let directory = seeded.directory.as_path();
    let journal: serde_json::Value = read_json(&directory.join(TURN_COMPLETION_FILE)).unwrap();
    let name = journal["event_file"].as_str().unwrap().to_owned();
    let event = directory.join(EVENTS_DIRECTORY).join(&name);
    corrupt_in_place(&event, "\"late");
    let event_bytes = fs::read(&event).unwrap();
    let set_aside = directory
        .join(EVENTS_DIRECTORY)
        .join(format!("{UNPUBLISHED_EVENT_PREFIX}{name}"));

    assert_eq!(close(directory).unwrap(), 1);
    assert_closed_and_settled(directory);
    assert!(!present(&directory.join(EVENTS_DIRECTORY), &name));
    assert_eq!(fs::read(&set_aside).unwrap(), event_bytes);
    let generation = status(directory).generation;

    assert_eq!(close(directory).unwrap(), 0);
    assert_closed_and_settled(directory);
    assert_eq!(status(directory).generation, generation);
    assert_eq!(fs::read(&set_aside).unwrap(), event_bytes);
}

/// A close that wrote its tombstone and stopped before settling the claim and journal.
fn interrupt_after_tombstone(directory: &Path) {
    update_status(
        directory,
        SessionState::Closed,
        None,
        Some("closed by the maintainer".to_owned()),
    )
    .unwrap();
}

#[test]
fn interrupted_close_converges_with_a_journal_that_is_not_utf8() {
    let seeded = seeded(JournaledEventState::Committed);
    let directory = seeded.directory.as_path();
    corrupt_in_place(&directory.join(TURN_COMPLETION_FILE), "\"late");
    interrupt_after_tombstone(directory);
    let events = crate::native::session::primitives::event_paths(directory).unwrap();

    assert!(recover_pending_completion(directory).unwrap());
    assert!(!present(directory, TURN_CLAIM_FILE));
    assert!(!present(directory, TURN_COMPLETION_FILE));
    assert!(!present(directory, LEGACY_RESUME_PENDING_FILE));
    assert_eq!(
        crate::native::session::primitives::event_paths(directory).unwrap(),
        events
    );
    assert!(!recover_pending_completion(directory).unwrap());
    assert_eq!(close(directory).unwrap(), 0);
    assert_closed_and_settled(directory);
}

#[test]
fn interrupted_close_converges_with_an_event_that_is_not_utf8() {
    let seeded = seeded(JournaledEventState::Committed);
    let directory = seeded.directory.as_path();
    let journal: serde_json::Value = read_json(&directory.join(TURN_COMPLETION_FILE)).unwrap();
    let name = journal["event_file"].as_str().unwrap().to_owned();
    corrupt_in_place(&directory.join(EVENTS_DIRECTORY).join(&name), "\"late");
    interrupt_after_tombstone(directory);

    assert!(recover_pending_completion(directory).unwrap());
    assert!(!present(directory, TURN_CLAIM_FILE));
    assert!(!present(directory, TURN_COMPLETION_FILE));
    assert!(!present(&directory.join(EVENTS_DIRECTORY), &name));
    assert!(present(
        &directory.join(EVENTS_DIRECTORY),
        &format!("{UNPUBLISHED_EVENT_PREFIX}{name}")
    ));
    assert!(!recover_pending_completion(directory).unwrap());
    assert_eq!(close(directory).unwrap(), 0);
    assert_closed_and_settled(directory);
}

/// An interrupted close's convergence refuses a dangling link at each record it checks for
/// before it removes the record, and leaves the link in place.
#[cfg(unix)]
#[test]
fn interrupted_close_convergence_over_a_dangling_link() {
    for name in [
        TURN_COMPLETION_FILE,
        TURN_CLAIM_FILE,
        LEGACY_RESUME_PENDING_FILE,
    ] {
        let seeded = seeded(JournaledEventState::Committed);
        let directory = seeded.directory.as_path();
        interrupt_after_tombstone(directory);
        let path = directory.join(name);
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(directory.join("missing-target"), &path).unwrap();
        let error = format!("{:#}", recover_pending_completion(directory).unwrap_err());
        assert!(
            error.contains("refusing non-regular session file") && error.contains(name),
            "{name}: {error}"
        );
        assert!(present(directory, name), "{name}");
    }
}

/// What an explicit close makes of a link at the records it checks for: the tombstone, the
/// active handle, the claimed close handle.
#[cfg(unix)]
#[test]
fn close_over_a_link_at_a_terminal_record() {
    let handle =
        serde_json::to_vec(&crate::native::session::test_support::lifecycle::close_test_terminal())
            .unwrap();
    let mut seen = Vec::new();
    for (name, dangling) in [
        (TERMINAL_TOMBSTONE_FILE, true),
        (TERMINAL_HANDLE_FILE, true),
        (TERMINAL_CLOSING_FILE, false),
        (TERMINAL_CLOSING_FILE, true),
    ] {
        let seeded = seeded(JournaledEventState::Committed);
        let directory = seeded.directory.as_path();
        let outside = tempfile::tempdir().unwrap();
        fs::remove_file(directory.join(TERMINAL_HANDLE_FILE)).unwrap();
        let target = outside.path().join(name);
        if !dangling {
            fs::write(&target, &handle).unwrap();
        }
        std::os::unix::fs::symlink(&target, directory.join(name)).unwrap();
        let outcome = match close(directory) {
            Ok(calls) => format!("closed, {calls} adapter call(s)"),
            Err(error) => {
                let text = format!("{error:#}");
                if text.contains("refusing non-regular session file") {
                    "refused".to_owned()
                } else {
                    format!(
                        "failed: {}",
                        text.replace(&directory.display().to_string(), "<dir>")
                    )
                }
            }
        };
        seen.push(format!(
            "{name} dangling={dangling}: {outcome}; status {}",
            status(directory).state.as_str()
        ));
    }
    // Each link is refused before the adapter runs. The claim moves a linked active handle
    // to the close handle's name, whose read refuses it and gives the handle back.
    let expected = [
        "terminal.closed.json dangling=true: refused; status ready",
        "terminal.json dangling=true: refused; status ready",
        "terminal.closing.json dangling=false: refused; status ready",
        "terminal.closing.json dangling=true: refused; status ready",
    ];
    assert_eq!(seen, expected);
}

// The consumption reason a macOS close carries from the tombstone into the closed status.
#[cfg(target_os = "macos")]
#[test]
fn close_reason_from_a_tombstone_that_is_not_utf8() {
    let seeded = seeded(JournaledEventState::Committed);
    let directory = seeded.directory.as_path();
    let tombstone = directory.join(TERMINAL_TOMBSTONE_FILE);
    fs::write(
        &tombstone,
        br#"{"consumed":true,"terminal":null,"reason":"kept"}"#,
    )
    .unwrap();
    corrupt_in_place(&tombstone, "\"kept");
    // The tombstone is not UTF-8, so it carries no reason; the close still finishes.
    assert_eq!(close(directory).unwrap(), 0);
    let closed = status(directory);
    assert_eq!(closed.state.as_str(), "closed");
    assert!(
        !closed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("kept")),
        "{:?}",
        closed.error
    );
}
