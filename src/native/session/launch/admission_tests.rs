//! The spawn fence and the wrapper's admission, each case one session directory: which
//! error each reports for every failing fact of the pending-launch judgment, alone and
//! together, and for each damaged record.
use super::test_support::{
    Change::{self, *},
    Expect, apply, pending_launch,
};
use super::*;
use std::{fs, time::Duration};

fn directory(changes: &[Change]) -> (tempfile::TempDir, Store) {
    let (directory, store, _) =
        pending_launch("session-", &std::env::temp_dir(), Duration::from_secs(30));
    apply(&store, changes);
    (directory, store)
}

const CANCELLED_LAUNCHING: &str = "provider launch cancelled: session is launching";
const CANCELLED_CLOSED: &str = "provider launch cancelled: session is closed";
const ATTEMPTED: &str = "provider launch already attempted; refusing a second spawn";
const CLAIM: &str = "provider launch claim no longer belongs to this wrapper";
const EXPIRED: &str = "provider launch deadline expired before spawn";

#[test]
fn spawn_fence_reports_the_first_failing_fact_in_one_order() {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut cases: Vec<(&str, Vec<Change>, Expect)> = vec![
        ("pending", vec![], Expect::Pass),
        (
            "status closed",
            vec![StatusClosed],
            Expect::Text(CANCELLED_CLOSED),
        ),
        (
            "status failed",
            vec![StatusFailed],
            Expect::Text("provider launch cancelled: session is failed"),
        ),
        ("status damaged", vec![StatusDamaged], Expect::Status),
        (
            "tombstone",
            vec![Tombstone],
            Expect::Text(CANCELLED_LAUNCHING),
        ),
        // The tombstone is read as a regular file and decoded.
        (
            "tombstone damaged",
            vec![TombstoneDamaged],
            Expect::Tombstone,
        ),
        (
            "tombstone directory",
            vec![TombstoneDirectory],
            Expect::Tombstone,
        ),
        ("receipt token", vec![ReceiptToken], Expect::Text(CLAIM)),
        ("spawning", vec![ReceiptSpawning], Expect::Text(ATTEMPTED)),
        ("expired", vec![ReceiptExpired], Expect::Text(EXPIRED)),
        ("claim absent", vec![ClaimAbsent], Expect::Text(CLAIM)),
        ("claim other", vec![ClaimOther], Expect::Text(CLAIM)),
        ("claim unreadable", vec![ClaimUnreadable], Expect::Claim),
        ("receipt damaged", vec![ReceiptDamaged], Expect::Receipt),
        (
            "receipt before status",
            vec![ReceiptDamaged, StatusDamaged],
            Expect::Receipt,
        ),
        (
            "status before tombstone",
            vec![Tombstone, StatusDamaged],
            Expect::Status,
        ),
        (
            "status before damaged tombstone",
            vec![StatusClosed, TombstoneDamaged],
            Expect::Text(CANCELLED_CLOSED),
        ),
        (
            "tombstone before phase",
            vec![Tombstone, ReceiptSpawning],
            Expect::Text(CANCELLED_LAUNCHING),
        ),
        (
            "phase before deadline",
            vec![ReceiptSpawning, ReceiptExpired],
            Expect::Text(ATTEMPTED),
        ),
        (
            "phase before unreadable claim",
            vec![ReceiptSpawning, ClaimUnreadable],
            Expect::Text(ATTEMPTED),
        ),
        // The deadline precedes the current Claim, which is read last.
        (
            "expired with another claim",
            vec![ReceiptExpired, ClaimOther],
            Expect::Text(EXPIRED),
        ),
        (
            "expired with unreadable claim",
            vec![ReceiptExpired, ClaimUnreadable],
            Expect::Text(EXPIRED),
        ),
        (
            "status before unreadable claim",
            vec![StatusClosed, ClaimUnreadable],
            Expect::Text(CANCELLED_CLOSED),
        ),
        // Without a receipt only the status and the tombstone are judged.
        ("receipt-less", vec![ReceiptMissing], Expect::Pass),
        (
            "receipt-less status closed",
            vec![ReceiptMissing, StatusClosed],
            Expect::Text(CANCELLED_CLOSED),
        ),
        (
            "receipt-less status damaged",
            vec![ReceiptMissing, StatusDamaged],
            Expect::Status,
        ),
        (
            "receipt-less tombstone",
            vec![ReceiptMissing, Tombstone],
            Expect::Text(CANCELLED_LAUNCHING),
        ),
        (
            "receipt-less damaged tombstone",
            vec![ReceiptMissing, TombstoneDamaged],
            Expect::Tombstone,
        ),
        (
            "receipt-less unreadable claim",
            vec![ReceiptMissing, ClaimUnreadable],
            Expect::Pass,
        ),
    ];
    #[cfg(unix)]
    cases.extend([
        ("tombstone link", vec![TombstoneLink], Expect::Tombstone),
        (
            "tombstone dangling link",
            vec![TombstoneDanglingLink],
            Expect::Tombstone,
        ),
    ]);
    let mut failures = Vec::new();
    for (case, changes, expect) in cases {
        let (_directory, store) = directory(&changes);
        let expected = expect.error(&store);
        // As `spawn` reads: the receipt first, then the fence under the lifecycle lock.
        let actual = read(&store)
            .and_then(|record| check_spawn_locked(&store, record.as_ref()))
            .err()
            .map(|error| format!("{error:#}"));
        if actual != expected {
            failures.push(format!("{case}: {actual:?} != {expected:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

const REFUSED: &str =
    "provider launch cancelled or already attempted; the recorded session is unchanged";

#[test]
fn wrapper_admission_reports_the_first_failing_fact_without_logging() {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut cases: Vec<(&str, Vec<Change>, Expect)> = vec![
        ("pending", vec![], Expect::Pass),
        ("status closed", vec![StatusClosed], Expect::Text(REFUSED)),
        ("status failed", vec![StatusFailed], Expect::Text(REFUSED)),
        ("status damaged", vec![StatusDamaged], Expect::Status),
        // Close's window: the tombstone refuses before the status says closed.
        ("tombstone", vec![Tombstone], Expect::Text(REFUSED)),
        (
            "tombstone damaged",
            vec![TombstoneDamaged],
            Expect::Tombstone,
        ),
        (
            "tombstone directory",
            vec![TombstoneDirectory],
            Expect::Tombstone,
        ),
        ("receipt token", vec![ReceiptToken], Expect::Text(REFUSED)),
        ("spawning", vec![ReceiptSpawning], Expect::Text(REFUSED)),
        // The wrapper has no deadline policy; the spawn fence judges the deadline.
        ("expired", vec![ReceiptExpired], Expect::Pass),
        ("claim absent", vec![ClaimAbsent], Expect::Text(REFUSED)),
        ("claim other", vec![ClaimOther], Expect::Text(REFUSED)),
        ("claim unreadable", vec![ClaimUnreadable], Expect::Claim),
        ("receipt damaged", vec![ReceiptDamaged], Expect::Receipt),
        (
            "receipt before status",
            vec![ReceiptDamaged, StatusDamaged],
            Expect::Receipt,
        ),
        (
            "status before tombstone",
            vec![Tombstone, StatusDamaged],
            Expect::Status,
        ),
        (
            "status before damaged tombstone",
            vec![StatusClosed, TombstoneDamaged],
            Expect::Text(REFUSED),
        ),
        (
            "phase before unreadable claim",
            vec![ReceiptSpawning, ClaimUnreadable],
            Expect::Text(REFUSED),
        ),
        (
            "status before unreadable claim",
            vec![StatusClosed, ClaimUnreadable],
            Expect::Text(REFUSED),
        ),
        (
            "tombstone before unreadable claim",
            vec![Tombstone, ClaimUnreadable],
            Expect::Text(REFUSED),
        ),
        (
            "expired with spawning",
            vec![ReceiptExpired, ReceiptSpawning],
            Expect::Text(REFUSED),
        ),
        // Without a receipt the status and tombstone still fence registration.
        ("receipt-less", vec![ReceiptMissing], Expect::Pass),
        (
            "receipt-less status damaged",
            vec![ReceiptMissing, StatusDamaged],
            Expect::Status,
        ),
        (
            "receipt-less tombstone",
            vec![ReceiptMissing, Tombstone],
            Expect::Text(REFUSED),
        ),
        (
            "receipt-less damaged tombstone",
            vec![ReceiptMissing, TombstoneDamaged],
            Expect::Tombstone,
        ),
    ];
    #[cfg(unix)]
    cases.push(("tombstone link", vec![TombstoneLink], Expect::Tombstone));
    let mut failures = Vec::new();
    for (case, changes, expect) in cases {
        let (_directory, store) = directory(&changes);
        let expected = expect.error(&store);
        let before_log = fs::read(store.directory().join(LOG)).unwrap();
        let actual = admit_wrapper(&store)
            .err()
            .map(|error| format!("{error:#}"));
        if actual != expected {
            failures.push(format!("{case}: {actual:?} != {expected:?}"));
        }
        assert_eq!(
            fs::read(store.directory().join(LOG)).unwrap(),
            before_log,
            "{case}"
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn wrapper_owner(store: &Store, pid: u32) -> NativeSessionOwner {
    NativeSessionOwner {
        pid,
        managed_session_id: Some(
            store
                .directory()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .into(),
        ),
        ..Default::default()
    }
}

fn launch_files(store: &Store) -> [Vec<u8>; 4] {
    [
        CoreRecord::Owner,
        CoreRecord::Status,
        CoreRecord::TurnClaim,
        CoreRecord::Launch,
    ]
    .map(|kind| fs::read(store.record(kind).path()).unwrap())
}

#[test]
fn registration_after_a_spawn_attempt_preserves_launch_files() {
    for phase in [Phase::Spawning, Phase::Spawned] {
        let (_directory, store) = directory(&[]);
        register_wrapper(&store, || Ok(wrapper_owner(&store, 10))).unwrap();
        let mut receipt = read(&store).unwrap().unwrap();
        receipt.phase = phase;
        store
            .record(CoreRecord::Launch)
            .write_json(&receipt)
            .unwrap();
        let before = launch_files(&store);
        let log = fs::read(store.directory().join(LOG)).unwrap();
        assert!(register_wrapper(&store, || Ok(wrapper_owner(&store, 20))).is_err());
        assert_eq!(launch_files(&store), before);
        assert_eq!(fs::read(store.directory().join(LOG)).unwrap(), log);
    }
}

#[test]
fn spawn_fence_selects_the_wrapper_and_only_it_settles() {
    let (_directory, store) = directory(&[]);
    let spawner = wrapper_owner(&store, 10);
    let other = wrapper_owner(&store, 20);
    register_wrapper(&store, || Ok(spawner.clone())).unwrap();
    register_wrapper(&store, || Ok(other.clone())).unwrap();

    let mut command = Command::new(std::env::current_exe().unwrap());
    command.arg("--list").stdout(std::process::Stdio::null());
    let mut child = spawn_for_wrapper(
        &store,
        &mut command,
        None,
        Some(&spawner),
        std::convert::identity,
        || {
            assert!(recorded_owner_matches(&store, &spawner)?);
            Ok(())
        },
        |_| update_status(store.directory(), SessionState::Running, None, None),
    )
    .unwrap();
    child.wait().unwrap();
    let before = launch_files(&store);
    let log = fs::read(store.directory().join(LOG)).unwrap();
    let refused = spawn_for_wrapper(
        &store,
        &mut command,
        None,
        Some(&other),
        std::convert::identity,
        || panic!("refused wrapper started a watcher"),
        |_| panic!("only one provider can start"),
    )
    .map(|_| ());
    assert!(refused.is_err());
    finalize_wrapper(&store, &other, &refused).unwrap();
    assert_eq!(launch_files(&store), before);

    assert_eq!(fs::read(store.directory().join(LOG)).unwrap(), log);
    finalize_wrapper(&store, &spawner, &Ok(())).unwrap();
    assert_eq!(store.status().unwrap().state, SessionState::Exited);
    assert!(turn::current_claim_token(&store).unwrap().is_none());
}

fn version_check_fixture() -> (tempfile::TempDir, Store) {
    let (directory, store) = directory(&[]);
    store
        .record(CoreRecord::Manifest)
        .write_json(&serde_json::json!({
            "schema": 1,
            "id": directory.path().file_name().unwrap().to_str().unwrap(),
            "provider": "codex",
            "provider_path": "unused",
            "provider_version": "unused",
            "workspace": ".",
            "title": "test",
            "yolo": false,
            "created_unix_ms": 1,
        }))
        .unwrap();
    (directory, store)
}

#[test]
fn version_failure_of_an_unrecorded_wrapper_preserves_the_spawned_launch() {
    let (_directory, store) = version_check_fixture();
    let spawner = wrapper_owner(&store, 10);
    let other = wrapper_owner(&store, 20);
    let mut before = None;
    let error = wrapper::run_registered(
        &store,
        || Ok(other.clone()),
        |_, _, _| {
            // The recorded snapshot at the time this version check returns.
            store.write_owner(&spawner)?;
            let mut receipt = read(&store)?.unwrap();
            receipt.phase = Phase::Spawned;
            store.record(CoreRecord::Launch).write_json(&receipt)?;
            update_status(store.directory(), SessionState::Running, None, None)?;
            before = Some((launch_files(&store), fs::read(store.directory().join(LOG))?));
            bail!("scripted version failure")
        },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "scripted version failure");
    assert_eq!(
        (
            launch_files(&store),
            fs::read(store.directory().join(LOG)).unwrap()
        ),
        before.unwrap()
    );
}

#[test]
fn owner_is_available_for_exit_repair_during_the_version_check() {
    let (_directory, store) = version_check_fixture();
    let owner = wrapper_owner(&store, 10);
    let error = wrapper::run_registered(
        &store,
        || Ok(owner.clone()),
        |_, _, bound| {
            assert!(bound.is_some());
            repair_with_owner(&store, |_| {
                assert!(recorded_owner_matches(&store, &owner)?);
                Ok(Some(super::super::owner::tests::reused_attestation()))
            })?;
            assert!(store.status()?.error.unwrap().contains("wrapper exited"));
            assert!(turn::current_claim_token(&store)?.is_none());
            bail!("scripted check ended")
        },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "scripted check ended");
}

#[test]
fn definitive_spawn_failure_settles_before_another_registration() {
    let (directory, store) = directory(&[]);
    let spawner = wrapper_owner(&store, 10);
    register_wrapper(&store, || Ok(spawner.clone())).unwrap();
    let mut command = Command::new(directory.path().join("missing-provider"));
    let error = spawn_for_wrapper(
        &store,
        &mut command,
        None,
        Some(&spawner),
        std::convert::identity,
        || Ok(()),
        |_| unreachable!(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("failed to spawn"));
    assert_eq!(read(&store).unwrap().unwrap().phase, Phase::Pending);
    assert_eq!(store.status().unwrap().state, SessionState::Failed);
    assert!(turn::current_claim_token(&store).unwrap().is_none());
    assert!(register_wrapper(&store, || Ok(wrapper_owner(&store, 20))).is_err());
    assert!(recorded_owner_matches(&store, &spawner).unwrap());
}

#[test]
fn watcher_start_failure_observes_pending_and_settles_under_the_fence() {
    let (directory, store) = directory(&[]);
    let owner = wrapper_owner(&store, 10);
    register_wrapper(&store, || Ok(owner.clone())).unwrap();
    let mut command = Command::new(directory.path().join("unused"));
    let error = spawn_for_wrapper(
        &store,
        &mut command,
        None,
        Some(&owner),
        std::convert::identity,
        || {
            assert_eq!(read(&store)?.unwrap().phase, Phase::Pending);
            bail!("watcher start failed")
        },
        |_| unreachable!(),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "watcher start failed");
    assert_eq!(
        store.status().unwrap().error.as_deref(),
        Some("watcher start failed")
    );
    assert_eq!(read(&store).unwrap().unwrap().phase, Phase::Pending);
    assert!(turn::current_claim_token(&store).unwrap().is_none());
}

#[test]
fn registration_identifies_without_the_lock_and_rechecks_admission() {
    let (_directory, store) = directory(&[]);
    let original = wrapper_owner(&store, 10);
    register_wrapper(&store, || Ok(original.clone())).unwrap();
    let error = register_wrapper(&store, || {
        let _lock = store.try_lock()?.expect("identification must be unlocked");
        update_status(store.directory(), SessionState::Running, None, None)?;
        bail!("identification failed")
    })
    .unwrap_err();
    assert!(error.to_string().contains("cancelled or already attempted"));
    assert!(recorded_owner_matches(&store, &original).unwrap());
}

#[test]
fn reopen_refusal_is_recorded_only_for_the_registered_owner() {
    let (_directory, store) = directory(&[]);
    let owner = wrapper_owner(&store, 10);
    store.write_owner(&owner).unwrap();
    let refusal = || anyhow::anyhow!("conversation already held");
    let other = wrapper_owner(&store, 20);
    let skipped = wrapper::reopen_refusal(&store, Some(&other), refusal()).unwrap();
    assert!(skipped.is::<WrapperRefusal>());
    assert!(!store.record(CoreRecord::ReopenRefusal).present().unwrap());
    let recorded = wrapper::reopen_refusal(&store, Some(&owner), refusal()).unwrap();
    assert!(!recorded.is::<WrapperRefusal>());
    let detail: serde_json::Value = store.record(CoreRecord::ReopenRefusal).json().unwrap();
    assert_eq!(detail["detail"], "conversation already held");
}

#[test]
fn recorded_wrapper_settlement_errors_keep_the_bootstrap_stderr() {
    for damaged in [CoreRecord::Owner, CoreRecord::Completion] {
        let (_directory, store) = version_check_fixture();
        let error = wrapper::run_registered(
            &store,
            || Ok(wrapper_owner(&store, 10)),
            |_, _, _| {
                if matches!(damaged, CoreRecord::Owner) {
                    store.record(damaged).write_json(&"malformed")?;
                } else {
                    fs::create_dir(store.record(damaged).path())?;
                }
                bail!("version check failed")
            },
        )
        .unwrap_err();
        assert!(!error.is::<WrapperRefusal>());
        let message = format!("{error:#}");
        assert!(message.contains("version check failed"), "{message}");
        assert!(message.contains("finalization also failed"), "{message}");
    }
}

const BEFORE_RENAME: &str = "renaming a temporary record over its final path";
const AFTER_RENAME: &str = "syncing a renamed record";

#[test]
fn registration_write_fault_settles_according_to_the_visible_owner() {
    for (fault, previous) in [
        (AFTER_RENAME, true),
        (BEFORE_RENAME, true),
        (BEFORE_RENAME, false),
    ] {
        let (_directory, store) = directory(&[]);
        let wrapper = wrapper_owner(&store, 10);
        let other = wrapper_owner(&store, 20);
        if previous {
            store.write_owner(&other).unwrap();
        }
        let error = crate::native::session::primitives::with_fault_at(fault, || {
            register_wrapper(&store, || Ok(wrapper.clone()))
        })
        .unwrap_err();
        let foreign = previous && fault == BEFORE_RENAME;
        assert_eq!(error.is::<WrapperRefusal>(), foreign, "{fault}");
        assert!(format!("{error:#}").contains(fault));
        assert_eq!(
            recorded_owner_matches(&store, &wrapper).unwrap(),
            fault == AFTER_RENAME
        );
        assert_eq!(recorded_owner_matches(&store, &other).unwrap(), foreign);
        let status = store.status().unwrap();
        assert_eq!(
            status.state,
            if foreign {
                SessionState::Launching
            } else {
                SessionState::Failed
            }
        );
        assert_eq!(status.exit_code, (!foreign).then_some(1));
        assert_eq!(
            turn::current_claim_token(&store).unwrap().is_some(),
            foreign
        );
        let log = fs::read_to_string(store.directory().join(LOG)).unwrap();
        assert_eq!(log.contains(WRAPPER_EXIT_CODE_MARKER), !foreign);
        if !foreign {
            assert!(status.error.unwrap().contains(fault));
            assert!(log.contains(fault));
        }
    }
}

#[test]
fn spawning_write_fault_settles_before_the_fence_returns() {
    for (fault, visible) in [
        (BEFORE_RENAME, Phase::Pending),
        (AFTER_RENAME, Phase::Spawning),
    ] {
        let (directory, store) = directory(&[]);
        let owner = wrapper_owner(&store, 10);
        register_wrapper(&store, || Ok(owner.clone())).unwrap();
        let error = crate::native::session::primitives::with_fault_at(fault, || {
            spawn_for_wrapper(
                &store,
                &mut Command::new(directory.path().join("unused")),
                None,
                Some(&owner),
                std::convert::identity,
                || Ok(()),
                |_| unreachable!(),
            )
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains(fault));
        assert_eq!(read(&store).unwrap().unwrap().phase, visible);
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        assert!(status.error.unwrap().contains(fault));
        assert_eq!(
            turn::current_claim_token(&store).unwrap().is_some(),
            visible == Phase::Spawning
        );
        assert!(admit_wrapper(&store).is_err());
    }
}

#[test]
fn pending_reset_write_fault_preserves_the_spawn_error_and_settles_under_lock() {
    for (fault, visible) in [
        (AFTER_RENAME, Phase::Pending),
        (BEFORE_RENAME, Phase::Spawning),
    ] {
        let (_directory, store) = directory(&[]);
        let owner = wrapper_owner(&store, 10);
        register_wrapper(&store, || Ok(owner.clone())).unwrap();
        let _lock = store.lock().unwrap();
        let mut record = read(&store).unwrap().unwrap();
        record.phase = Phase::Spawning;
        store
            .record(CoreRecord::Launch)
            .write_json(&record)
            .unwrap();
        let error = crate::native::session::primitives::with_fault_at(fault, || {
            reset_failed_spawn_locked(
                &store,
                Some(&mut record),
                Some(&owner),
                anyhow::anyhow!("OS spawn failed"),
            )
        });
        assert!(format!("{error:#}").contains(fault));
        assert!(format!("{error:#}").contains("OS spawn failed"));
        assert_eq!(read(&store).unwrap().unwrap().phase, visible);
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        let reason = status.error.unwrap();
        assert!(
            reason.contains("OS spawn failed") && reason.contains(fault),
            "{reason}"
        );
        assert_eq!(
            turn::current_claim_token(&store).unwrap().is_some(),
            visible == Phase::Spawning
        );
        assert!(admit_wrapper(&store).is_err());
    }
}

#[test]
fn fence_owner_write_fault_settles_only_if_the_spawner_is_recorded() {
    for fault in [AFTER_RENAME, BEFORE_RENAME] {
        let (directory, store) = directory(&[]);
        let spawner = wrapper_owner(&store, 10);
        let other = wrapper_owner(&store, 20);
        register_wrapper(&store, || Ok(other.clone())).unwrap();
        let error = crate::native::session::primitives::with_fault_at(fault, || {
            spawn_for_wrapper(
                &store,
                &mut Command::new(directory.path().join("unused")),
                None,
                Some(&spawner),
                |error| error.context("provider startup"),
                || unreachable!(),
                |_| unreachable!(),
            )
        })
        .unwrap_err();
        let ours = fault == AFTER_RENAME;
        assert_eq!(recorded_owner_matches(&store, &spawner).unwrap(), ours);
        assert_eq!(recorded_owner_matches(&store, &other).unwrap(), !ours);
        let status = store.status().unwrap();
        assert_eq!(
            status.state,
            if ours {
                SessionState::Failed
            } else {
                SessionState::Launching
            }
        );
        if ours {
            assert_eq!(status.error.unwrap(), format!("{error:#}"));
        }
        assert_eq!(turn::current_claim_token(&store).unwrap().is_none(), ours);
    }
}
