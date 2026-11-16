//! Launch-owned host diagnostics. Writers take the lifecycle lock themselves; readers
//! attribute history to the receipt, never to a later live claim.
use super::{Phase, Record};
use crate::native::session::primitives::unix_ms;
use crate::native::session::state::SessionState;
use crate::native::session::{CoreRecord, Reader, Store, record, turn};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub(in crate::native) const LIMIT: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(in crate::native) enum Terminal {
    Iterm2,
    Wezterm,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(in crate::native) enum Stage {
    AwaitingHost,
    HostStarted,
    EnvironmentPreparing,
    EnvironmentReady,
    LaunchShellStarting,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::native) enum FailureReason {
    IdentityUnavailable,
    BindingMismatch,
    BindingTimeout,
    LaunchBudgetExhausted,
    CaptureSpawnFailed,
    CaptureTimeout,
    CaptureExitNonzero,
    CaptureSignaled,
    CaptureNoOutput,
    CaptureOversized,
    CaptureInvalid,
    CleanupFailed,
    InputFlushFailed,
    LaunchShellExecFailed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(in crate::native) struct Failure {
    pub(in crate::native) reason: FailureReason,
    pub(in crate::native) exit_code: Option<i32>,
    pub(in crate::native) signal: Option<i32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(in crate::native) struct HostIdentity {
    pub(in crate::native) pid: u32,
    pub(in crate::native) start_seconds: u64,
    pub(in crate::native) start_microseconds: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(in crate::native) struct HostRecord {
    pub(in crate::native) schema: u32,
    pub(in crate::native) managed_session_id: String,
    pub(in crate::native) claim_token: String,
    pub(in crate::native) terminal: Terminal,
    pub(in crate::native) stage: Stage,
    pub(in crate::native) host: Option<HostIdentity>,
    pub(in crate::native) updated_unix_ms: u128,
    pub(in crate::native) elapsed_ms: u64,
    pub(in crate::native) capture_budget_ms: u64,
    pub(in crate::native) capture_elapsed_ms: Option<u64>,
    pub(in crate::native) variables: Option<u32>,
    pub(in crate::native) background_children_stopped: Option<u32>,
    pub(in crate::native) failure: Option<Failure>,
}

#[derive(Debug)]
pub(in crate::native) enum HostObservation {
    NotRecorded,
    Unreadable(String),
    SchemaUnsupported(u32),
    IdentityMismatch,
    Recorded(HostRecord),
}

pub(in crate::native) fn observe(reader: &Reader, receipt: Option<&Record>) -> HostObservation {
    let bytes = match reader.record(CoreRecord::LaunchHost).timeline(LIMIT) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return HostObservation::NotRecorded,
        Err(error) => return HostObservation::Unreadable(format!("{error:#}")),
    };
    match record::launch_host(&bytes) {
        Ok(HostObservation::Recorded(host)) => {
            if receipt.is_none_or(|r| r.claim_token != host.claim_token)
                || reader.directory().file_name().and_then(|n| n.to_str())
                    != Some(&host.managed_session_id)
            {
                HostObservation::IdentityMismatch
            } else {
                HostObservation::Recorded(host)
            }
        }
        Ok(other) => other,
        Err(error) => HostObservation::Unreadable(error.to_string()),
    }
}

fn eligible(store: &Store, token: &str, require_time: bool) -> Result<Record> {
    let receipt = super::read(store)?.context("missing launch receipt")?;
    if receipt.claim_token != token
        || receipt.phase != Phase::Pending
        || (require_time && unix_ms() >= receipt.deadline_unix_ms)
        || store.status()?.state != SessionState::Launching
        || store.closed_if_present()?.is_some()
        || turn::current_claim_token(store)?.as_deref() != Some(token)
    {
        bail!("launch host is no longer eligible");
    }
    Ok(receipt)
}

fn write(store: &Store, host: &HostRecord) -> Result<()> {
    let bytes = serde_json::to_vec(host)?;
    record::launch_host(&bytes).map_err(anyhow::Error::from)?;
    store.record(CoreRecord::LaunchHost).write_json(host)
}

pub(in crate::native) fn initialize(store: &Store, token: &str, terminal: Terminal) -> Result<()> {
    let _lock = store.lock()?;
    eligible(store, token, true)?;
    if store
        .record(CoreRecord::LaunchHost)
        .timeline(LIMIT)?
        .is_some()
    {
        bail!("launch host is already recorded");
    }
    write(
        store,
        &HostRecord {
            schema: 1,
            managed_session_id: store
                .directory()
                .file_name()
                .and_then(|n| n.to_str())
                .context("session directory has no id")?
                .into(),
            claim_token: token.into(),
            terminal,
            stage: Stage::AwaitingHost,
            host: None,
            updated_unix_ms: unix_ms(),
            elapsed_ms: 0,
            capture_budget_ms: 0,
            capture_elapsed_ms: None,
            variables: None,
            background_children_stopped: None,
            failure: None,
        },
    )
}

fn current(store: &Store, token: &str, require_time: bool) -> Result<HostRecord> {
    let receipt = eligible(store, token, require_time)?;
    match observe(store, Some(&receipt)) {
        HostObservation::Recorded(host) if host.failure.is_none() => Ok(host),
        _ => bail!("launch host record is unavailable or terminal"),
    }
}

/// Even the same incarnation cannot admit twice. Callers must retain the first result.
pub(in crate::native) fn admit(store: &Store, token: &str, identity: HostIdentity) -> Result<()> {
    let _lock = store.lock()?;
    let mut host = current(store, token, true)?;
    if host.host.is_some() || host.stage != Stage::AwaitingHost {
        bail!("duplicate launch host");
    }
    host.host = Some(identity);
    host.stage = Stage::HostStarted;
    host.updated_unix_ms = unix_ms();
    write(store, &host)
}

/// Only counters enter this API; neither environment values nor startup output can enter.
#[derive(Default)]
pub(in crate::native) struct Progress {
    pub(in crate::native) elapsed_ms: u64,
    pub(in crate::native) capture_budget_ms: u64,
    pub(in crate::native) capture_elapsed_ms: Option<u64>,
    pub(in crate::native) variables: Option<u32>,
    pub(in crate::native) background_children_stopped: Option<u32>,
}

pub(in crate::native) fn advance(
    store: &Store,
    token: &str,
    identity: HostIdentity,
    stage: Stage,
    progress: Progress,
) -> Result<()> {
    let _lock = store.lock()?;
    let mut host = current(store, token, true)?;
    if host.host != Some(identity) || stage <= host.stage {
        bail!("stale launch host transition");
    }
    host.stage = stage;
    host.updated_unix_ms = unix_ms();
    host.elapsed_ms = progress.elapsed_ms;
    host.capture_budget_ms = progress.capture_budget_ms;
    host.capture_elapsed_ms = progress.capture_elapsed_ms;
    host.variables = progress.variables;
    host.background_children_stopped = progress.background_children_stopped;
    write(store, &host)
}

/// `None` can record identity acquisition failure before admission, but cannot write
/// over an admitted incarnation. Failure is final and preserves the last stage.
/// Expiry withholds launch authority but does not discard its failure diagnosis;
/// the current pending claim and unsettled, unclosed session are still required.
pub(in crate::native) fn fail(
    store: &Store,
    token: &str,
    identity: Option<HostIdentity>,
    failure: Failure,
    elapsed_ms: u64,
    capture_elapsed_ms: Option<u64>,
) -> Result<()> {
    let _lock = store.lock()?;
    let mut host = current(store, token, false)?;
    if host.host != identity {
        bail!("stale launch host failure");
    }
    host.failure = Some(failure);
    host.elapsed_ms = elapsed_ms;
    if capture_elapsed_ms.is_some() {
        host.capture_elapsed_ms = capture_elapsed_ms;
    }
    host.updated_unix_ms = unix_ms();
    write(store, &host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn fixture() -> (tempfile::TempDir, Store, String) {
        let dir = tempfile::Builder::new()
            .prefix("session-")
            .tempdir()
            .unwrap();
        std::fs::create_dir(dir.path().join("events")).unwrap();
        crate::native::session::update_status(dir.path(), SessionState::Launching, None, None)
            .unwrap();
        let store = Store::open_unchecked(dir.path());
        let claim = turn::claim(&store, &[]).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        super::super::begin(&store, &token, Instant::now() + Duration::from_secs(30)).unwrap();
        (dir, store, token)
    }
    fn identity() -> HostIdentity {
        HostIdentity {
            pid: 123,
            start_seconds: 1234,
            start_microseconds: 123,
        }
    }
    fn read(store: &Store) -> HostRecord {
        match observe(store, super::super::read(store).unwrap().as_ref()) {
            HostObservation::Recorded(h) => h,
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn admission_forward_progress_and_failure_are_immutable() {
        let (_dir, store, token) = fixture();
        assert!(matches!(
            observe(&store, None),
            HostObservation::NotRecorded
        ));
        initialize(&store, &token, Terminal::Iterm2).unwrap();
        assert!(initialize(&store, &token, Terminal::Iterm2).is_err());
        assert_eq!(read(&store).stage, Stage::AwaitingHost);
        admit(&store, &token, identity()).unwrap();
        let before = store.record(CoreRecord::LaunchHost).bytes().unwrap();
        assert!(admit(&store, &token, identity()).is_err());
        assert!(admit(&store, "another", identity()).is_err());
        assert_eq!(
            before,
            store.record(CoreRecord::LaunchHost).bytes().unwrap()
        );
        for stage in [
            Stage::EnvironmentPreparing,
            Stage::EnvironmentReady,
            Stage::LaunchShellStarting,
        ] {
            advance(&store, &token, identity(), stage, Progress::default()).unwrap();
            assert_eq!(read(&store).stage, stage);
            assert!(advance(&store, &token, identity(), stage, Progress::default()).is_err());
            assert!(
                advance(
                    &store,
                    &token,
                    identity(),
                    Stage::HostStarted,
                    Progress::default()
                )
                .is_err()
            );
        }
        let failure = Failure {
            reason: FailureReason::LaunchShellExecFailed,
            exit_code: None,
            signal: None,
        };
        fail(
            &store,
            &token,
            Some(identity()),
            failure.clone(),
            44,
            Some(40),
        )
        .unwrap();
        assert_eq!(read(&store).stage, Stage::LaunchShellStarting);
        assert_eq!(read(&store).capture_elapsed_ms, Some(40));
        assert_eq!(read(&store).failure, Some(failure.clone()));
        assert!(fail(&store, &token, Some(identity()), failure, 55, None).is_err());
        assert!(admit(&store, &token, identity()).is_err());
    }
    #[test]
    fn stale_writers_cannot_change_settled_closed_or_replaced_launches() {
        for case in [
            "settled",
            "closed",
            "claim",
            "receipt",
            "spawned",
            "incarnation",
        ] {
            let (_dir, store, token) = fixture();
            initialize(&store, &token, Terminal::Wezterm).unwrap();
            admit(&store, &token, identity()).unwrap();
            let mut id = identity();
            match case {
                "settled" => crate::native::session::update_status(
                    store.directory(),
                    SessionState::Failed,
                    None,
                    None,
                )
                .unwrap(),
                "closed" => store.write_closed(&store.status().unwrap()).unwrap(),
                "claim" => std::fs::write(store.record(CoreRecord::TurnClaim).path(), b"successor")
                    .unwrap(),
                "receipt" | "spawned" => {
                    let mut receipt = super::super::read(&store).unwrap().unwrap();
                    if case == "receipt" {
                        receipt.claim_token = "successor".into();
                    }
                    if case == "spawned" {
                        receipt.phase = Phase::Spawned;
                    }
                    store
                        .record(CoreRecord::Launch)
                        .write_json(&receipt)
                        .unwrap();
                }
                "incarnation" => id.start_microseconds += 1,
                _ => unreachable!(),
            }
            let bytes = store.record(CoreRecord::LaunchHost).bytes().unwrap();
            assert!(
                advance(
                    &store,
                    &token,
                    id,
                    Stage::EnvironmentPreparing,
                    Progress::default()
                )
                .is_err(),
                "{case}"
            );
            assert!(
                fail(
                    &store,
                    &token,
                    Some(id),
                    Failure {
                        reason: FailureReason::CaptureTimeout,
                        exit_code: None,
                        signal: None
                    },
                    1,
                    None
                )
                .is_err(),
                "{case}"
            );
            assert!(admit(&store, &token, id).is_err());
            assert_eq!(bytes, store.record(CoreRecord::LaunchHost).bytes().unwrap());
        }
    }
    #[test]
    fn historical_observation_uses_receipt_and_session_identity() {
        let (_dir, store, token) = fixture();
        initialize(&store, &token, Terminal::Iterm2).unwrap();
        std::fs::write(store.record(CoreRecord::TurnClaim).path(), b"later").unwrap();
        assert_eq!(read(&store).claim_token, token);
        assert!(matches!(
            observe(&store, None),
            HostObservation::IdentityMismatch
        ));
        let mut receipt = super::super::read(&store).unwrap().unwrap();
        receipt.claim_token = "other".into();
        assert!(matches!(
            observe(&store, Some(&receipt)),
            HostObservation::IdentityMismatch
        ));
        let mut host = read(&store);
        host.managed_session_id = "other".into();
        store
            .record(CoreRecord::LaunchHost)
            .write_json(&host)
            .unwrap();
        assert!(matches!(
            observe(&store, super::super::read(&store).unwrap().as_ref()),
            HostObservation::IdentityMismatch
        ));
    }
    #[test]
    fn decoder_is_bounded_typed_and_reports_future_schema() {
        let (_dir, store, token) = fixture();
        initialize(&store, &token, Terminal::Iterm2).unwrap();
        let host = read(&store);
        for (key, value) in [
            ("terminal", serde_json::json!("ghostty")),
            ("stage", serde_json::json!("ready")),
            ("updated_unix_ms", serde_json::json!(-1)),
            ("claim_token", serde_json::json!("")),
            (
                "host",
                serde_json::json!({"pid": 1, "start_seconds": 1, "start_microseconds": 0}),
            ),
            (
                "failure",
                serde_json::json!({"reason": "arbitrary", "exit_code": null, "signal": null}),
            ),
        ] {
            let mut value_host = serde_json::to_value(&host).unwrap();
            value_host[key] = value;
            assert!(
                record::launch_host(&serde_json::to_vec(&value_host).unwrap()).is_err(),
                "{key}"
            );
        }
        for bytes in [vec![b' '; LIMIT as usize + 1], vec![0xff], b"{}".to_vec()] {
            assert!(record::launch_host(&bytes).is_err());
            std::fs::write(store.record(CoreRecord::LaunchHost).path(), &bytes).unwrap();
            assert!(matches!(
                observe(&store, None),
                HostObservation::Unreadable(_)
            ));
        }
        assert!(matches!(
            record::launch_host(br#"{"schema":2}"#).unwrap(),
            HostObservation::SchemaUnsupported(2)
        ));
    }
    #[test]
    fn expired_current_launch_can_record_failure_but_cannot_advance() {
        let (_dir, store, token) = fixture();
        initialize(&store, &token, Terminal::Iterm2).unwrap();
        admit(&store, &token, identity()).unwrap();
        let mut receipt = super::super::read(&store).unwrap().unwrap();
        receipt.deadline_unix_ms = 0;
        store
            .record(CoreRecord::Launch)
            .write_json(&receipt)
            .unwrap();
        assert!(
            advance(
                &store,
                &token,
                identity(),
                Stage::EnvironmentPreparing,
                Progress::default()
            )
            .is_err()
        );
        fail(
            &store,
            &token,
            Some(identity()),
            Failure {
                reason: FailureReason::BindingTimeout,
                exit_code: None,
                signal: None,
            },
            30000,
            None,
        )
        .unwrap();
        assert_eq!(read(&store).stage, Stage::HostStarted);
        assert_eq!(
            read(&store).failure.unwrap().reason,
            FailureReason::BindingTimeout
        );
    }
    #[test]
    fn concurrent_admission_has_exactly_one_winner() {
        let (_dir, store, token) = fixture();
        initialize(&store, &token, Terminal::Iterm2).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let wins = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..2)
                .map(|n| {
                    let barrier = &barrier;
                    let store = &store;
                    let token = &token;
                    scope.spawn(move || {
                        let mut id = identity();
                        id.pid += n;
                        barrier.wait();
                        admit(store, token, id).is_ok()
                    })
                })
                .collect();
            threads
                .into_iter()
                .filter(|t| t.thread().id() != std::thread::current().id())
                .map(|t| usize::from(t.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(wins, 1);
    }
}
