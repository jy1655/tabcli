use super::support::*;
use super::*;
use crate::native::prune::prune_closed_sessions;
use crate::native::session::SessionStatus;
use crate::native::session::primitives::{
    read_json, remove_file_if_present, rename_session_file, state_root_durability_receipt_present,
    write_json_atomic, write_private,
};
use crate::native::session::test_support::{
    STATE_ROOT_ANCESTRY_SYNC_LIMIT, STATE_ROOT_DURABLE_FILE, SyncRecord, TERMINAL_CLOSING_FILE,
    TURN_CLAIM_FILE, TURN_COMPLETION_FILE, injected_fault, injected_sync_failure,
    with_fault_budget, with_sync_failure, with_sync_log,
};
use crate::native::sessions::sessions_in;
use crate::native::{
    CreatedSession, NativeCommand, SessionSpec, create_session_within, parse_args, session,
};
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn atomic_json_write_syncs_temporary_then_persisted_file_then_parent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("record.json");
    let (outcome, log) = with_sync_log(|| write_json_atomic(&path, &serde_json::json!({})));
    outcome.unwrap();

    assert_eq!(log.len(), 3, "{log:?}");
    match &log[0] {
        SyncRecord::File(temporary) => {
            assert_eq!(temporary.parent(), Some(directory.path()));
            assert_ne!(temporary, &path);
            assert!(temporary.extension().is_some_and(|ext| ext == "tmp"));
        }
        other => panic!("first sync was {other:?}"),
    }
    assert_eq!(log[1], SyncRecord::File(path.clone()));
    assert_eq!(
        log[2],
        SyncRecord::Directory(directory.path().to_path_buf())
    );
}

#[test]
fn private_write_claim_creation_and_removal_sync_file_then_parent() {
    let directory = tempfile::tempdir().unwrap();
    let private = directory.path().join("initial-prompt.txt");
    let (outcome, log) = with_sync_log(|| write_private(&private, b"prompt"));
    outcome.unwrap();
    assert_eq!(
        log,
        [
            SyncRecord::File(private.clone()),
            SyncRecord::Directory(directory.path().to_path_buf()),
        ]
    );

    let claim_path = directory.path().join(TURN_CLAIM_FILE);
    let (outcome, log) =
        with_sync_log(|| turn::claim(&Store::open_unchecked(directory.path()), &[]));
    outcome.unwrap().retain();
    assert_eq!(
        log[..2],
        [
            SyncRecord::File(claim_path.clone()),
            SyncRecord::Directory(directory.path().to_path_buf()),
        ]
    );

    let (outcome, log) = with_sync_log(|| remove_file_if_present(&private));
    outcome.unwrap();
    assert_eq!(log, [SyncRecord::Directory(directory.path().to_path_buf())]);
    let (outcome, log) = with_sync_log(|| remove_file_if_present(&private));
    outcome.unwrap();
    assert!(
        log.is_empty(),
        "a missing file must not be reported as synced"
    );
}

#[test]
fn session_directory_creation_syncs_the_state_root_before_its_records() {
    let root = tempfile::tempdir().unwrap();
    // The root's parent is taken as durable, so the ancestry walk syncs it once and stops.
    let durable = [root.path().parent().unwrap().to_path_buf()];
    let (outcome, log) = with_sync_log(|| {
        create_session_within(
            root.path(),
            &durable,
            SessionSpec {
                provider: FirstPartyCli::Codex,
                provider_path: PathBuf::from("codex"),
                provider_version: "0.147.0".to_owned(),
                workspace: root.path().to_path_buf(),
                title: "durability".to_owned(),
                model: None,
                effort: None,
                yolo: false,
                prompt: "prompt".to_owned(),
            },
        )
    });
    let created = outcome.unwrap();

    // The root's own entry, then its durability receipt (which syncs the root), then the
    // root again for the session entry, then the session directory, before any record.
    assert_eq!(
        log.first(),
        Some(&SyncRecord::Directory(durable[0].clone()))
    );
    let receipt_index = log
        .iter()
        .position(|record| *record == SyncRecord::File(root.path().join(STATE_ROOT_DURABLE_FILE)))
        .expect("receipt sync");
    assert_eq!(
        log[receipt_index + 1..receipt_index + 4],
        [
            SyncRecord::Directory(root.path().to_path_buf()),
            SyncRecord::Directory(root.path().to_path_buf()),
            SyncRecord::Directory(created.directory.clone()),
        ],
        "{log:?}"
    );
    assert!(created.directory.join("events").is_dir());
    let manifest_index = log
        .iter()
        .position(|record| *record == SyncRecord::File(created.directory.join("manifest.json")))
        .expect("manifest sync");
    assert!(manifest_index > receipt_index + 3);
    let status: SessionStatus = read_json(&created.directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "launching");
    assert_eq!(status.error, None);
}

/// A state root two levels below a pre-existing base directory, with the base taken as
/// durable so the ancestry walk stops there.
struct NestedStateRoot {
    base: tempfile::TempDir,
    ancestor: PathBuf,
    root: PathBuf,
}

impl NestedStateRoot {
    fn new() -> Self {
        let base = tempfile::tempdir().unwrap();
        let ancestor = base.path().join("custom");
        let root = ancestor.join("native-sessions");
        Self {
            base,
            ancestor,
            root,
        }
    }

    fn spec(&self) -> SessionSpec {
        SessionSpec {
            provider: FirstPartyCli::Codex,
            provider_path: PathBuf::from("codex"),
            provider_version: "0.147.0".to_owned(),
            workspace: self.base.path().to_path_buf(),
            title: "durability".to_owned(),
            model: None,
            effort: None,
            yolo: false,
            prompt: "prompt".to_owned(),
        }
    }

    fn durable(&self) -> Vec<PathBuf> {
        vec![self.base.path().to_path_buf()]
    }

    fn create(&self) -> Result<CreatedSession> {
        create_session_within(&self.root, &self.durable(), self.spec())
    }

    fn receipt(&self) -> PathBuf {
        self.root.join(STATE_ROOT_DURABLE_FILE)
    }

    fn receipt_present(&self) -> bool {
        state_root_durability_receipt_present(&self.root)
    }

    /// The sync log of a creation that establishes the ancestry: the root's entry in its
    /// parent, the parent's entry in the base, the receipt (a temporary file, the receipt
    /// itself, then the root), the root again for the session entry, then the session.
    fn assert_established_ancestry(&self, log: &[SyncRecord], session: &Path) {
        assert_eq!(
            log[..2],
            [
                SyncRecord::Directory(self.ancestor.clone()),
                SyncRecord::Directory(self.base.path().to_path_buf()),
            ],
            "{log:?}"
        );
        assert!(
            matches!(&log[2], SyncRecord::File(temporary) if temporary.parent() == Some(self.root.as_path())),
            "{log:?}"
        );
        assert_eq!(
            log[3..7],
            [
                SyncRecord::File(self.receipt()),
                SyncRecord::Directory(self.root.clone()),
                SyncRecord::Directory(self.root.clone()),
                SyncRecord::Directory(session.to_path_buf()),
            ],
            "{log:?}"
        );
    }

    /// The sync log of a creation that found the receipt: the root's entry for the new
    /// session directory only, nothing above the root and no receipt.
    fn assert_trusted_receipt(&self, log: &[SyncRecord], session: &Path) {
        assert_eq!(
            log[..2],
            [
                SyncRecord::Directory(self.root.clone()),
                SyncRecord::Directory(session.to_path_buf()),
            ],
            "{log:?}"
        );
        assert!(
            !log.contains(&SyncRecord::Directory(self.ancestor.clone())),
            "{log:?}"
        );
        assert!(
            !log.contains(&SyncRecord::Directory(self.base.path().to_path_buf())),
            "{log:?}"
        );
        assert!(!log.contains(&SyncRecord::File(self.receipt())), "{log:?}");
    }
}

#[test]
fn session_directory_creation_syncs_newly_created_state_root_ancestors() {
    let nested = NestedStateRoot::new();
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();

    // Nearest entry first: the root's entry in its parent, then the parent's entry in the
    // pre-existing base, then the receipt, then the root itself for the session entry.
    nested.assert_established_ancestry(&log, &created.directory);
    assert!(created.directory.join("events").is_dir());
    assert!(nested.receipt_present());

    // A root with its receipt syncs nothing above itself.
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();
    nested.assert_trusted_receipt(&log, &created.directory);
}

const STATE_ROOT_ANCESTRY_SYNC_LABEL: &str = "syncing the state root's ancestry";

#[test]
fn a_later_creator_establishes_the_ancestry_its_stopped_creator_left_unsynced() {
    // Creator A stops right before the ancestry syncs: the root and its ancestor exist,
    // nothing was synced, and no receipt claims otherwise.
    let nested = NestedStateRoot::new();
    let (outcome, log) = with_sync_log(|| with_fault_budget(0, || nested.create()));
    let error = outcome.err().expect("creator A stopped");
    assert!(injected_fault(&error), "{error:#}");
    assert!(
        format!("{error:#}").contains(STATE_ROOT_ANCESTRY_SYNC_LABEL),
        "{error:#}"
    );
    assert!(log.is_empty(), "{log:?}");
    assert!(nested.root.is_dir());
    assert!(!nested.receipt_present());

    // Creator B finds an existing root and still performs every ancestry sync before it
    // writes the receipt and its own session.
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();
    nested.assert_established_ancestry(&log, &created.directory);
    assert!(nested.receipt_present());
    let status: SessionStatus = read_json(&created.directory.join("status.json")).unwrap();
    assert_eq!((status.state.as_str(), status.error), ("launching", None));

    // With the receipt present, creator C syncs only the root entry for its session.
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();
    nested.assert_trusted_receipt(&log, &created.directory);

    // Every stop of creator A: the receipt exists only after both ancestry syncs, and a
    // creator that finds no receipt redoes the walk however far A got.
    for budget in 0.. {
        let nested = NestedStateRoot::new();
        let (outcome, a_log) = with_sync_log(|| with_fault_budget(budget, || nested.create()));
        let receipt_after_a = nested.receipt_present();
        // A stop right after the receipt's rename leaves an unsynced receipt behind, which
        // is safe: both ancestry syncs precede the first write under the root, so a
        // receipt that survives never outlives the durability it attests to.
        if receipt_after_a {
            assert_eq!(
                a_log[..2],
                [
                    SyncRecord::Directory(nested.ancestor.clone()),
                    SyncRecord::Directory(nested.base.path().to_path_buf()),
                ],
                "budget {budget}: the receipt was written before the ancestry syncs: {a_log:?}"
            );
        }
        let (outcome_b, b_log) = with_sync_log(|| nested.create());
        let created_b = outcome_b.unwrap();
        assert!(nested.receipt_present(), "budget {budget}");
        if receipt_after_a {
            nested.assert_trusted_receipt(&b_log, &created_b.directory);
        } else {
            nested.assert_established_ancestry(&b_log, &created_b.directory);
        }
        match outcome {
            Ok(_) => {
                assert!(receipt_after_a, "budget {budget}");
                break;
            }
            Err(error) => assert!(injected_fault(&error), "budget {budget}: {error:#}"),
        }
    }
}

#[test]
fn the_ancestry_walk_covers_ancestors_this_creator_did_not_make() {
    // Another creator made the ancestor and stopped before making the root or syncing
    // anything. This creator makes only the root, yet the ancestor's entry in the base is
    // synced as well: the walk is over the root's ancestry, not over what was created here.
    let nested = NestedStateRoot::new();
    fs::create_dir(&nested.ancestor).unwrap();
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();
    nested.assert_established_ancestry(&log, &created.directory);
    assert!(nested.receipt_present());
}

#[test]
fn the_ancestry_walk_is_bounded() {
    let base = tempfile::tempdir().unwrap();
    let mut levels = Vec::new();
    let mut root = base.path().to_path_buf();
    for level in 1..=18 {
        root = root.join(format!("l{level}"));
        levels.push(root.clone());
    }
    root = root.join("native-sessions");
    let durable = [base.path().to_path_buf()];
    let (outcome, log) = with_sync_log(|| {
        create_session_within(
            &root,
            &durable,
            SessionSpec {
                provider: FirstPartyCli::Codex,
                provider_path: PathBuf::from("codex"),
                provider_version: "0.147.0".to_owned(),
                workspace: base.path().to_path_buf(),
                title: "durability".to_owned(),
                model: None,
                effort: None,
                yolo: false,
                prompt: "prompt".to_owned(),
            },
        )
    });
    outcome.unwrap();
    let receipt_index = log
        .iter()
        .position(|record| *record == SyncRecord::File(root.join(STATE_ROOT_DURABLE_FILE)))
        .expect("receipt sync");
    let walked: Vec<&SyncRecord> = log[..receipt_index]
        .iter()
        .filter(|record| matches!(record, SyncRecord::Directory(_)))
        .collect();
    // The nearest sixteen holders: l18 (the root's entry) down to l3; l2, l1, and the
    // base are beyond the bound.
    let expected: Vec<SyncRecord> = levels
        .iter()
        .rev()
        .take(STATE_ROOT_ANCESTRY_SYNC_LIMIT)
        .map(|level| SyncRecord::Directory(level.clone()))
        .collect();
    assert_eq!(walked.len(), STATE_ROOT_ANCESTRY_SYNC_LIMIT, "{log:?}");
    assert!(
        walked
            .iter()
            .zip(&expected)
            .all(|(walked, expected)| *walked == expected),
        "{log:?}"
    );
    assert!(
        !log.contains(&SyncRecord::Directory(levels[1].clone())),
        "{log:?}"
    );
    assert!(
        !log.contains(&SyncRecord::Directory(base.path().to_path_buf())),
        "{log:?}"
    );
    assert!(state_root_durability_receipt_present(&root));
}

#[test]
fn an_ancestry_sync_failure_never_blocks_session_creation_and_leaves_no_receipt() {
    let nested = NestedStateRoot::new();
    let (outcome, log) =
        with_sync_failure(nested.base.path(), || with_sync_log(|| nested.create()));
    let created = outcome.unwrap();
    // Both syncs were attempted, in order; the failure stopped the walk before the receipt.
    assert_eq!(
        log[..2],
        [
            SyncRecord::Directory(nested.ancestor.clone()),
            SyncRecord::Directory(nested.base.path().to_path_buf()),
        ],
        "{log:?}"
    );
    assert!(
        !log.contains(&SyncRecord::File(nested.receipt())),
        "{log:?}"
    );
    assert_eq!(
        log[2],
        SyncRecord::Directory(nested.root.clone()),
        "{log:?}"
    );
    assert!(!nested.receipt_present());
    assert!(created.directory.join("manifest.json").is_file());
    let status: SessionStatus = read_json(&created.directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "launching");
    let error = status
        .error
        .expect("the launch status records the failed walk");
    assert!(
        error.contains("state root ancestry was not made durable"),
        "{error}"
    );
    assert!(error.contains("injected sync failure for"), "{error}");
    assert!(
        error.contains(&nested.base.path().display().to_string()),
        "{error}"
    );

    // The next creation walks again, and this time writes the receipt.
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();
    nested.assert_established_ancestry(&log, &created.directory);
    assert!(nested.receipt_present());
    let status: SessionStatus = read_json(&created.directory.join("status.json")).unwrap();
    assert_eq!(status.error, None);
}

#[test]
fn a_non_regular_receipt_is_not_trusted() {
    // A directory at the receipt's path proves nothing; the walk runs, and the receipt
    // write then fails loudly rather than silently accepting the impostor.
    let nested = NestedStateRoot::new();
    fs::create_dir_all(nested.receipt()).unwrap();
    let (outcome, log) = with_sync_log(|| nested.create());
    let error = outcome.err().expect("the receipt write fails");
    assert!(!injected_sync_failure(&error), "{error:#}");
    assert_eq!(
        log[..2],
        [
            SyncRecord::Directory(nested.ancestor.clone()),
            SyncRecord::Directory(nested.base.path().to_path_buf()),
        ],
        "{log:?}"
    );
    assert!(!nested.receipt_present());
}

#[test]
fn record_rename_syncs_the_destination_directory_then_a_different_source_directory() {
    let directory = tempfile::tempdir().unwrap();
    let from = directory.path().join("terminal.json");
    let to = directory.path().join(TERMINAL_CLOSING_FILE);
    fs::write(&from, "{}").unwrap();
    let (outcome, log) = with_sync_log(|| rename_session_file(&from, &to));
    outcome.unwrap();
    assert_eq!(log, [SyncRecord::Directory(directory.path().to_path_buf())]);

    let other = directory.path().join("events");
    fs::create_dir(&other).unwrap();
    let moved = other.join(TERMINAL_CLOSING_FILE);
    let (outcome, log) = with_sync_log(|| rename_session_file(&to, &moved));
    outcome.unwrap();
    assert_eq!(
        log,
        [
            SyncRecord::Directory(other.clone()),
            SyncRecord::Directory(directory.path().to_path_buf()),
        ]
    );
    assert!(moved.exists());
}

#[test]
fn state_root_faults_preserve_repair_and_prune_interleaving() {
    use session::enumerate::{TestEntry, with_entries};
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-repair");
    seed_event_written_completion_with(&directory, "result", "result");
    let entries = |directory: &Path| {
        vec![
            Ok(TestEntry {
                id: directory.file_name().unwrap().to_str().unwrap().into(),
                directory: directory.into(),
                directory_kind: Ok(true),
            }),
            Err(std::io::Error::other("injected entry failure")),
        ]
    };
    let NativeCommand::Sessions(request) = parse_args(["sessions", "--json"]).unwrap() else {
        panic!()
    };
    let error =
        with_entries(entries(&directory), || sessions_in(root.path(), &request)).unwrap_err();
    assert_eq!(error.to_string(), "injected entry failure");
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    let closed = write_prune_test_session(root.path(), "session-prune", "closed", Some(1));
    let error =
        with_entries(entries(&closed), || prune_closed_sessions(root.path(), 2)).unwrap_err();
    assert_eq!(error.to_string(), "injected entry failure");
    assert!(!closed.exists());
}
