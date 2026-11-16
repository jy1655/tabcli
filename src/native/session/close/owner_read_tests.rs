//! What Repair, prune and the macOS close-intent readers answer for Owner records that no
//! writer produces.
use super::*;
#[cfg(target_os = "macos")]
use crate::native::session::owner::damaged_tests::Damaged;
use crate::native::session::owner::damaged_tests::{
    DAMAGED, owner_path, read_error, write_damaged_owner,
};
use crate::native::session::test_support::lifecycle::{
    reaped_child_pid, write_owned_terminal_state,
};

fn outcome<T: std::fmt::Debug>(result: Result<T>) -> String {
    match result {
        Ok(value) => format!("ok {value:?}"),
        Err(error) => format!("error {error:#}"),
    }
}

fn session_owner(directory: &Path, pid: u32) -> NativeSessionOwner {
    NativeSessionOwner {
        pid,
        managed_session_id: Some(directory.file_name().unwrap().to_str().unwrap().to_owned()),
        ..NativeSessionOwner::default()
    }
}

// Repair attests the owner it reads; the injected liveness answers live, so Repair leaves an
// attested owner's session untouched.
#[test]
fn repair_owner_read_answers_for_each_damaged_shape() {
    for &damaged in DAMAGED {
        let directory = tempfile::tempdir().unwrap();
        write_owned_terminal_state(directory.path(), "working", std::process::id());
        let path = owner_path(directory.path());
        write_damaged_owner(
            directory.path(),
            &session_owner(directory.path(), std::process::id()),
            damaged,
        );
        let attested = std::cell::Cell::new(0);
        let observed = outcome(repair_dead_owner_with(
            &Store::open_unchecked(directory.path()),
            |_| {
                attested.set(attested.get() + 1);
                Ok(true)
            },
            |_| panic!("a live owner retains nothing"),
            |_| panic!("a live owner's surface is not closed"),
            budget::close_deadline(),
        ));
        // Repair reads the record under the one read policy: it no longer follows a link.
        let expected = (format!("error {}", read_error(&path, damaged)), 0);
        assert_eq!((observed, attested.get()), expected, "{damaged:?}");
    }
}

// Prune is blocked by any owner it cannot show gone: an unreadable record blocks it.
#[test]
fn prune_owner_read_answers_for_each_damaged_shape() {
    for &damaged in DAMAGED {
        let directory = tempfile::tempdir().unwrap();
        write_damaged_owner(
            directory.path(),
            &session_owner(directory.path(), reaped_child_pid()),
            damaged,
        );
        let blocked = Reader::open_unchecked(directory.path())
            .native_owner_blocks_prune()
            .unwrap();
        assert!(blocked, "{damaged:?}");
    }
}

#[cfg(target_os = "macos")]
fn attested_session_with_intent(directory: &Path, teardown: bool) -> terminal::TerminalSession {
    let owner = crate::native::tests::write_attested_apple_terminal_state(directory, "ready", 4242);
    let handle: terminal::TerminalSession = crate::native::session::primitives::read_json(
        &directory.join(crate::native::session::names::TERMINAL_HANDLE_FILE),
    )
    .unwrap();
    let store = Store::open_unchecked(directory);
    if teardown {
        record_terminal_teardown_intent(
            &store,
            "session-owner123",
            &handle,
            &owner,
            4242,
            "characterization teardown",
        )
        .unwrap();
    } else {
        record_terminal_close_intent(&store, "session-owner123", &handle, &owner).unwrap();
    }
    handle
}

// The teardown intent is the first record the macOS close authority reads; it validates the
// intent against the owner record.
#[cfg(target_os = "macos")]
#[test]
fn teardown_intent_reads_a_linked_owner_record() {
    let directory = tempfile::tempdir().unwrap();
    let handle = attested_session_with_intent(directory.path(), true);
    let owner = Reader::open_unchecked(directory.path()).owner().unwrap();
    write_damaged_owner(directory.path(), &owner, Damaged::Linked);
    let observed = outcome(
        terminal_teardown_intent(&Reader::open_unchecked(directory.path()), &handle)
            .map(|t| t.map(|t| t.process_group)),
    );
    assert_eq!(
        observed,
        format!(
            "error {}",
            read_error(&owner_path(directory.path()), Damaged::Linked)
        )
    );
}

// The close intent resumes only the exact owner it names; an owner record that cannot be
// decoded grants nothing.
#[cfg(target_os = "macos")]
#[test]
fn close_intent_owner_read_answers_for_each_damaged_shape() {
    for &damaged in DAMAGED {
        let directory = tempfile::tempdir().unwrap();
        let handle = attested_session_with_intent(directory.path(), false);
        let path = owner_path(directory.path());
        let owner = Reader::open_unchecked(directory.path()).owner().unwrap();
        write_damaged_owner(directory.path(), &owner, damaged);
        let observed = outcome(
            terminal_close_intent_owner(&Reader::open_unchecked(directory.path()), &handle, true)
                .map(|o| o.map(|o| o.pid)),
        );
        assert_eq!(
            observed,
            format!("error {}", read_error(&path, damaged)),
            "{damaged:?}"
        );
    }
}

// The close authority's presence check, read before the launch record and status.
#[cfg(target_os = "macos")]
#[test]
fn close_authority_owner_presence_answers_for_each_damaged_shape() {
    use crate::native::terminal::close_authority::{CloseObservation, Production};
    let directory = tempfile::tempdir().unwrap();
    let handle = attested_session_with_intent(directory.path(), false);
    let deadline = budget::close_deadline();
    let present = |directory: &Path| {
        outcome(Production::new(directory, "session-owner123", &handle, deadline).owner_present())
    };
    fs::remove_file(owner_path(directory.path())).unwrap();
    assert_eq!(present(directory.path()), "ok false");
    for &damaged in DAMAGED {
        let path = owner_path(directory.path());
        let _ = fs::remove_dir(&path);
        write_damaged_owner(
            directory.path(),
            &session_owner(directory.path(), 4242),
            damaged,
        );
        assert_eq!(
            present(directory.path()),
            format!("error {}", read_error(&path, damaged)),
            "{damaged:?}"
        );
    }
}
