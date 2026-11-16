//! Session directories in the state that the launch admission checks judge: one pending
//! launch fixture, and the record changes that make each fact of that judgment fail.
use super::{Phase, Record};
use crate::native::session::names::{
    CLOSED_STATUS_FILE, LAUNCH_FILE, STATUS_FILE, TURN_CLAIM_FILE,
};
use crate::native::session::{
    CoreRecord, Reader, RecordStore, SessionState, SessionStatus, Store, turn, update_status,
};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

/// A session directory as the launcher leaves it before it opens the surface: private to
/// the current user, launching, claimed, with a pending launch receipt for that Claim.
pub(in crate::native) fn pending_launch(
    prefix: &str,
    parent: &Path,
    timeout: Duration,
) -> (tempfile::TempDir, Store, String) {
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(parent)
        .unwrap();
    // The macOS bound hosts admit only a directory private to the current user. Windows
    // tests keep the temporary directory's default access list.
    #[cfg(unix)]
    crate::native::session::primitives::set_private_directory_permissions(directory.path())
        .unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Launching, None, None).unwrap();
    let store = Store::open_unchecked(directory.path());
    let claim = turn::claim(&store, &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    super::begin(&store, &token, Instant::now() + timeout).unwrap();
    (directory, store, token)
}

/// One record change that makes one fact of the pending-launch judgment fail, or damages
/// the record the fact is read from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum Change {
    StatusClosed,
    StatusFailed,
    StatusDamaged,
    /// Close's window: `closed.json` is written, `status.json` still says launching.
    Tombstone,
    TombstoneDamaged,
    TombstoneDirectory,
    #[cfg(unix)]
    TombstoneLink,
    #[cfg(unix)]
    TombstoneDanglingLink,
    ReceiptToken,
    ReceiptSpawning,
    ReceiptExpired,
    ReceiptDamaged,
    ReceiptMissing,
    ClaimAbsent,
    ClaimOther,
    ClaimUnreadable,
}

fn receipt(store: &Store, change: impl FnOnce(&mut Record)) {
    let mut record = super::read(store).unwrap().unwrap();
    change(&mut record);
    store
        .record(CoreRecord::Launch)
        .write_json(&record)
        .unwrap();
}

// The tombstone Close writes before `status.json`: the next generation, closed.
fn closed(status: &SessionStatus) -> SessionStatus {
    SessionStatus {
        state: SessionState::Closed,
        generation: status.generation + 1,
        updated_unix_ms: status.updated_unix_ms,
        exit_code: None,
        error: None,
        residual_surface: None,
    }
}

/// Applies `changes` in order; a damaging change comes after the changes that still read
/// the record it damages.
pub(in crate::native) fn apply(store: &Store, changes: &[Change]) {
    let directory = store.directory();
    for change in changes {
        match change {
            Change::StatusClosed => {
                update_status(directory, SessionState::Closed, None, None).unwrap()
            }
            Change::StatusFailed => {
                update_status(directory, SessionState::Failed, None, None).unwrap()
            }
            Change::StatusDamaged => fs::write(directory.join(STATUS_FILE), b"{").unwrap(),
            Change::Tombstone => store
                .record(CoreRecord::Closed)
                .write_json(&closed(&store.status().unwrap()))
                .unwrap(),
            Change::TombstoneDamaged => {
                fs::write(directory.join(CLOSED_STATUS_FILE), b"{").unwrap()
            }
            Change::TombstoneDirectory => {
                fs::create_dir(directory.join(CLOSED_STATUS_FILE)).unwrap()
            }
            #[cfg(unix)]
            Change::TombstoneLink => {
                let target = directory.join("closed-target.json");
                RecordStore::at(&target)
                    .write_json(&closed(&store.status().unwrap()))
                    .unwrap();
                std::os::unix::fs::symlink(target, directory.join(CLOSED_STATUS_FILE)).unwrap();
            }
            #[cfg(unix)]
            Change::TombstoneDanglingLink => std::os::unix::fs::symlink(
                directory.join("missing-closed.json"),
                directory.join(CLOSED_STATUS_FILE),
            )
            .unwrap(),
            Change::ReceiptToken => receipt(store, |r| r.claim_token = "successor".into()),
            Change::ReceiptSpawning => receipt(store, |r| r.phase = Phase::Spawning),
            Change::ReceiptExpired => receipt(store, |r| r.deadline_unix_ms = 0),
            Change::ReceiptDamaged => fs::write(directory.join(LAUNCH_FILE), b"{").unwrap(),
            Change::ReceiptMissing => fs::remove_file(directory.join(LAUNCH_FILE)).unwrap(),
            Change::ClaimAbsent => fs::remove_file(directory.join(TURN_CLAIM_FILE)).unwrap(),
            Change::ClaimOther => {
                fs::remove_file(directory.join(TURN_CLAIM_FILE)).unwrap();
                RecordStore::at(&directory.join(TURN_CLAIM_FILE))
                    .write_private(b"successor")
                    .unwrap();
            }
            Change::ClaimUnreadable => {
                fs::remove_file(directory.join(TURN_CLAIM_FILE)).unwrap();
                fs::create_dir(directory.join(TURN_CLAIM_FILE)).unwrap();
            }
        }
    }
}

/// The exact errors the session's readers return for its records as they are now.
pub(in crate::native) fn status_error(reader: &Reader) -> String {
    format!("{:#}", reader.status().unwrap_err())
}
pub(in crate::native) fn tombstone_error(reader: &Reader) -> String {
    format!("{:#}", reader.closed_if_present().unwrap_err())
}
pub(in crate::native) fn claim_error(reader: &Reader) -> String {
    format!("{:#}", turn::current_claim_token(reader).unwrap_err())
}
pub(in crate::native) fn receipt_error(reader: &Reader) -> String {
    format!("{:#}", super::read(reader).unwrap_err())
}

/// What a launch admission check reports for a session directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum Expect {
    Pass,
    Text(&'static str),
    Status,
    Tombstone,
    Claim,
    Receipt,
}

impl Expect {
    /// The exact error (`{:#}`) expected from the directory as it is now; `None` passes.
    pub(in crate::native) fn error(&self, reader: &Reader) -> Option<String> {
        match self {
            Self::Pass => None,
            Self::Text(text) => Some((*text).to_owned()),
            Self::Status => Some(status_error(reader)),
            Self::Tombstone => Some(tombstone_error(reader)),
            Self::Claim => Some(claim_error(reader)),
            Self::Receipt => Some(receipt_error(reader)),
        }
    }
}
