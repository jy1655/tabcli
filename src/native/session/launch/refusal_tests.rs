//! The settlement of a launch whose session directory the launcher refused before it created
//! any surface: it settles only the launch it refused, never through a path that names
//! another session, and a reader holding the lifecycle lock does not leave it pending.
use super::*;
use crate::native::session::launch::launcher::settle_open_failure;
use std::{fs, os::unix::fs::PermissionsExt, sync::mpsc, thread};

// A kind whose launch host this platform runs and the launcher judges.
#[cfg(target_os = "macos")]
const KIND: terminal::TerminalKind = terminal::TerminalKind::Iterm2;
#[cfg(target_os = "linux")]
const KIND: terminal::TerminalKind = terminal::TerminalKind::Tmux;

fn records(store: &Store) -> Vec<Option<Vec<u8>>> {
    let mut records = [
        CoreRecord::Status,
        CoreRecord::TurnClaim,
        CoreRecord::Launch,
    ]
    .map(|record| store.record(record).bytes().unwrap())
    .to_vec();
    records.push(fs::read(store.directory().join(LOG)).ok());
    records
}

// The path of session A is replaced by a link to session B, which is still launching. A's
// admission refuses the link; settling that refusal must not fail B or release B's Claim.
#[test]
fn a_refusal_never_settles_another_session_through_the_refused_path() {
    let root = tempfile::tempdir().unwrap();
    let (a_directory, a, a_token) =
        test_support::pending_launch("session-a", root.path(), START_TIMEOUT);
    let (_b_directory, b, b_token) =
        test_support::pending_launch("session-b", root.path(), START_TIMEOUT);
    let path = a.directory().to_owned();
    fs::rename(&path, root.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(b.directory(), &path).unwrap();
    let refusal = terminal::refuse_before_launch(KIND, &path).unwrap_err();
    let before = records(&b);
    let error = settle_open_failure(&path, &a_token, refusal);
    assert!(
        format!("{error:#}").contains("refusing non-directory"),
        "{error:#}"
    );
    assert_eq!(records(&b), before);
    assert_eq!(b.status().unwrap().state, SessionState::Launching);
    assert_eq!(
        turn::current_claim_token(&b).unwrap().as_deref(),
        Some(b_token.as_str())
    );
    drop(a_directory);
}

// `inspect --timeline` holds the shared lifecycle lock while the launcher refuses the session
// directory. The refusal is known before any surface exists, so the settlement waits for the
// reader instead of leaving the launch pending with its Claim installed.
#[test]
fn a_known_refusal_settles_after_a_reader_releases_the_lifecycle_lock() {
    let (_directory, store, token) =
        test_support::pending_launch("session-reader", &std::env::temp_dir(), START_TIMEOUT);
    fs::set_permissions(store.directory(), fs::Permissions::from_mode(0o770)).unwrap();
    let refusal = terminal::refuse_before_launch(KIND, store.directory()).unwrap_err();
    let reason = format!(
        "terminal launch failed: {refusal:#}; no surface handle was recorded and no prompt was sent"
    );
    let (held, holding) = mpsc::channel();
    let (settled, released) = mpsc::channel::<()>();
    let path = store.directory().to_owned();
    let reader = thread::spawn(move || {
        let lock = Reader::open_unchecked(&path).lock_shared().unwrap();
        assert!(lock.is_some());
        held.send(()).unwrap();
        // Held until the settlement returns, or for two seconds while it waits.
        let _ = released.recv_timeout(Duration::from_secs(2));
        drop(lock);
    });
    holding.recv().unwrap();
    let error = settle_open_failure(store.directory(), &token, refusal);
    let _ = settled.send(());
    reader.join().unwrap();
    assert!(
        !format!("{error:#}").contains("the claim is retained"),
        "{error:#}"
    );
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(status.error.as_deref(), Some(reason.as_str()));
    assert!(turn::current_claim_token(&store).unwrap().is_none());
    assert_eq!(read(&store).unwrap().unwrap().phase, Phase::Pending);
    assert!(
        fs::read_to_string(store.directory().join(LOG))
            .unwrap()
            .contains(&reason)
    );
}
