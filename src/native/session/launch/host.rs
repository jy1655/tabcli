//! Launch-owned host diagnostics. Writers take the lifecycle lock themselves; readers
//! attribute history to the receipt, never to a later live claim.
use super::Record;
use super::pending::{self, Expiry};
use crate::native::session::primitives::unix_ms;
use crate::native::session::{CoreRecord, Reader, Store, record};
use crate::native::terminal::TerminalKind;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub(in crate::native) const LIMIT: u64 = 16 * 1024;

const LAUNCH_HOST_COUNT: usize = {
    let mut count = 0;
    let mut index = 0;
    while index < TerminalKind::ALL.len() {
        if TerminalKind::ALL[index].has_launch_host_form() {
            count += 1;
        }
        index += 1;
    }
    count
};

/// The wire names of the kinds that use the launch host, in declaration order: the only
/// terminals a host record can name.
const LAUNCH_HOST_NAMES: [&str; LAUNCH_HOST_COUNT] = {
    let mut names = [""; LAUNCH_HOST_COUNT];
    let mut index = 0;
    let mut next = 0;
    while index < TerminalKind::ALL.len() {
        let kind = TerminalKind::ALL[index];
        if kind.has_launch_host_form() {
            names[next] = kind.as_str();
            next += 1;
        }
        index += 1;
    }
    names
};

/// The record's `terminal`: a kind that uses the launch host, read as an externally tagged
/// unit variant through the same deserializer calls that a derived enum makes, so that a
/// refused value gets serde's own message (`unknown variant ..., expected ...`) naming the
/// accepted kinds, at the same position.
fn launch_host_kind<'de, D>(deserializer: D) -> std::result::Result<TerminalKind, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{DeserializeSeed, EnumAccess, Error, VariantAccess, Visitor};
    use std::fmt;
    struct Name;
    impl<'de> Visitor<'de> for Name {
        type Value = TerminalKind;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("variant identifier")
        }
        fn visit_str<E: Error>(self, value: &str) -> std::result::Result<TerminalKind, E> {
            TerminalKind::ALL
                .into_iter()
                .find(|kind| kind.has_launch_host_form() && kind.as_str() == value)
                .ok_or_else(|| E::unknown_variant(value, &LAUNCH_HOST_NAMES))
        }
    }
    impl<'de> DeserializeSeed<'de> for Name {
        type Value = TerminalKind;
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> std::result::Result<TerminalKind, D::Error> {
            deserializer.deserialize_identifier(self)
        }
    }
    struct Kind;
    impl<'de> Visitor<'de> for Kind {
        type Value = TerminalKind;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a terminal kind that uses the launch host")
        }
        fn visit_enum<A: EnumAccess<'de>>(
            self,
            data: A,
        ) -> std::result::Result<TerminalKind, A::Error> {
            let (kind, variant) = data.variant_seed(Name)?;
            variant.unit_variant()?;
            Ok(kind)
        }
    }
    deserializer.deserialize_enum("TerminalKind", &LAUNCH_HOST_NAMES, Kind)
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

/// Who must act on a launch host failure: the one grouping of `FailureReason`, which
/// status.error and doctor each map to their own words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::native) enum Responsibility {
    /// The user's zsh startup files did not produce an environment without a terminal.
    StartupFiles,
    /// The launch deadline left no time for the step.
    Deadline,
    /// The terminal did not identify the surface TAB created.
    SurfaceIdentity,
    /// TAB's own host step could not complete.
    HostStep,
}

impl FailureReason {
    pub(in crate::native) fn responsibility(self) -> Responsibility {
        use FailureReason::*;
        match self {
            CaptureTimeout | CaptureExitNonzero | CaptureSignaled | CaptureNoOutput
            | CaptureOversized | CaptureInvalid => Responsibility::StartupFiles,
            LaunchBudgetExhausted | BindingTimeout => Responsibility::Deadline,
            IdentityUnavailable | BindingMismatch => Responsibility::SurfaceIdentity,
            CaptureSpawnFailed | CleanupFailed | InputFlushFailed | LaunchShellExecFailed => {
                Responsibility::HostStep
            }
        }
    }

    /// The reason's record word, as every renderer and the launch log name it.
    pub(in crate::native) fn word(self) -> String {
        word(self)
    }
}

impl Stage {
    /// The stage's record word, as every renderer names it.
    pub(in crate::native) fn word(self) -> String {
        word(self)
    }
}

fn word(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .expect("enum serialization")
        .as_str()
        .expect("string enum")
        .into()
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
    #[serde(deserialize_with = "launch_host_kind")]
    pub(in crate::native) terminal: TerminalKind,
    pub(in crate::native) stage: Stage,
    pub(in crate::native) host: Option<HostIdentity>,
    pub(in crate::native) updated_unix_ms: u128,
    pub(in crate::native) elapsed_ms: u64,
    /// The host's estimate while `EnvironmentPreparing`, made before the capture starts; the
    /// budget the capture enforced from `EnvironmentReady` on, and in the failure record of a
    /// capture that ended the launch.
    pub(in crate::native) capture_budget_ms: u64,
    pub(in crate::native) capture_elapsed_ms: Option<u64>,
    pub(in crate::native) variables: Option<u32>,
    pub(in crate::native) background_children_stopped: Option<u32>,
    pub(in crate::native) failure: Option<Failure>,
}

impl HostRecord {
    /// What a failure adds to its reason: only a capture timeout has a detail, the budget
    /// that the startup files exceeded.
    pub(in crate::native) fn failure_detail(&self) -> Option<String> {
        self.failure
            .as_ref()
            .filter(|failure| failure.reason == FailureReason::CaptureTimeout)
            .map(|_| {
                format!(
                    "startup files did not finish within {} ms",
                    self.capture_budget_ms
                )
            })
    }

    /// The record's fields that both the timeline and doctor show. Neither the identities
    /// nor `updated_unix_ms` are among them: each renderer places the update time itself.
    pub(in crate::native) fn evidence(&self) -> serde_json::Value {
        serde_json::json!({
            "terminal": self.terminal,
            "stage": self.stage,
            "failure": self.failure,
            "elapsed_ms": self.elapsed_ms,
            "capture_budget_ms": self.capture_budget_ms,
            "capture_elapsed_ms": self.capture_elapsed_ms,
            "variables": self.variables,
            "background_children_stopped": self.background_children_stopped,
        })
    }
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
    judge(
        reader
            .record(CoreRecord::LaunchHost)
            .timeline(LIMIT)
            .map_err(|error| format!("{error:#}")),
        receipt,
        reader
            .directory()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default(),
    )
}

/// The one judgment of retained bytes, shared by `observe` and readers that already hold
/// the bytes (timeline): the receipt attributes history, never a later live claim.
pub(in crate::native) fn judge(
    bytes: std::result::Result<Option<Vec<u8>>, String>,
    receipt: Option<&Record>,
    session_id: &str,
) -> HostObservation {
    let bytes = match bytes {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return HostObservation::NotRecorded,
        Err(error) => return HostObservation::Unreadable(error),
    };
    match record::launch_host(&bytes) {
        Ok(HostObservation::Recorded(host)) => {
            if receipt.is_none_or(|r| r.claim_token != host.claim_token)
                || host.managed_session_id != session_id
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

#[derive(Debug)]
pub(in crate::native) struct ReceiptExpired;

impl std::fmt::Display for ReceiptExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("launch host is no longer eligible")
    }
}

impl std::error::Error for ReceiptExpired {}

// The Pending launch of `token`; `require_time` adds the receipt's deadline.
fn eligible(store: &Store, token: &str, require_time: bool) -> Result<Record> {
    let receipt = super::read(store)?.context("missing launch receipt")?;
    let expiry = if require_time {
        Expiry::Receipt
    } else {
        Expiry::Unchecked
    };
    match pending::judge(store, &receipt, token, expiry)? {
        Ok(()) => Ok(receipt),
        Err(pending::NotPending::Expired) => Err(ReceiptExpired.into()),
        Err(_) => bail!("launch host is no longer eligible"),
    }
}

fn write(store: &Store, host: &HostRecord) -> Result<()> {
    let bytes = serde_json::to_vec(host)?;
    record::launch_host(&bytes).map_err(anyhow::Error::from)?;
    store.record(CoreRecord::LaunchHost).write_json(host)
}

pub(super) fn initialize_for_launch(
    store: &Store,
    token: &str,
    terminal: TerminalKind,
) -> Result<()> {
    if terminal.records_launch_stages() {
        initialize(store, token, terminal)?;
    }
    Ok(())
}

pub(in crate::native) fn initialize(
    store: &Store,
    token: &str,
    terminal: TerminalKind,
) -> Result<()> {
    let _lock = store.lock()?;
    eligible(store, token, true)?;
    write(store, &unrecorded_host(store, token, terminal)?)
}

fn unrecorded_host(store: &Store, token: &str, terminal: TerminalKind) -> Result<HostRecord> {
    if store
        .record(CoreRecord::LaunchHost)
        .timeline(LIMIT)?
        .is_some()
    {
        bail!("launch host is already recorded");
    }
    Ok(HostRecord {
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
    })
}

/// Publish a failure after the Herdr host proved its address, PID and birth against the
/// binding. The Surface record holds its incarnation, so schema 1 carries no separate
/// host admission or capture stages.
/// A later Claim, close, wrapper spawn or binding change wins over this diagnosis.
#[cfg(any(target_os = "linux", windows, all(unix, test)))]
pub(in crate::native) fn fail_bound_herdr(
    store: &Store,
    token: &str,
    surface: &crate::native::terminal::TerminalSession,
    reason: FailureReason,
    elapsed_ms: u64,
) -> Result<()> {
    let _lock = store.lock()?;
    eligible(store, token, false)?;
    let mut host = unrecorded_host(store, token, TerminalKind::Herdr)?;
    anyhow::ensure!(
        surface.kind == TerminalKind::Herdr
            && pending::bound_surface(store, &host.managed_session_id, || "invalid host binding")?
                .as_ref()
                == Some(surface),
        "Herdr binding changed"
    );
    host.failure = Some(Failure {
        reason,
        exit_code: None,
        signal: None,
    });
    host.elapsed_ms = elapsed_ms;
    write(store, &host)
}

// `current` and the stage writers below serve the macOS capture host.
#[cfg(any(target_os = "macos", test))]
fn current(store: &Store, token: &str, require_time: bool) -> Result<HostRecord> {
    let receipt = eligible(store, token, require_time)?;
    match observe(store, Some(&receipt)) {
        HostObservation::Recorded(host) if host.failure.is_none() => Ok(host),
        _ => bail!("launch host record is unavailable or terminal"),
    }
}

/// The admitted host `identity` may still act for the Pending launch of `token`: the launch
/// is pending before its deadline, and the host record names that incarnation and no
/// failure. The terminal host checks this under the lifecycle lock before each act.
#[cfg(target_os = "macos")]
pub(in crate::native) fn eligible_incarnation(
    store: &Store,
    token: &str,
    identity: HostIdentity,
) -> Result<()> {
    let receipt = eligible(store, token, true)?;
    match observe(store, Some(&receipt)) {
        HostObservation::Recorded(host)
            if host.host == Some(identity) && host.failure.is_none() =>
        {
            Ok(())
        }
        _ => bail!("launch host is no longer eligible"),
    }
}

/// Even the same incarnation cannot admit twice. Callers must retain the first result.
#[cfg(any(target_os = "macos", test))]
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
/// `capture_budget_ms` is what `HostRecord::capture_budget_ms` holds at the stage.
#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
pub(in crate::native) struct Progress {
    pub(in crate::native) elapsed_ms: u64,
    pub(in crate::native) capture_budget_ms: u64,
    pub(in crate::native) capture_elapsed_ms: Option<u64>,
    pub(in crate::native) variables: Option<u32>,
    pub(in crate::native) background_children_stopped: Option<u32>,
}

#[cfg(any(target_os = "macos", test))]
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
/// The capture's elapsed time and budget, when given, replace the recorded ones; otherwise
/// the record keeps them as the last stage wrote them.
#[cfg(any(target_os = "macos", test))]
pub(in crate::native) fn fail(
    store: &Store,
    token: &str,
    identity: Option<HostIdentity>,
    failure: Failure,
    elapsed_ms: u64,
    capture_elapsed_ms: Option<u64>,
    capture_budget_ms: Option<u64>,
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
    if let Some(budget_ms) = capture_budget_ms {
        host.capture_budget_ms = budget_ms;
    }
    host.updated_unix_ms = unix_ms();
    write(store, &host)
}

#[cfg(test)]
mod tests {
    use super::super::Phase;
    use super::*;
    use crate::native::session::SessionState;
    use std::time::Duration;
    fn fixture() -> (tempfile::TempDir, Store, String) {
        super::super::test_support::pending_launch(
            "session-",
            &std::env::temp_dir(),
            Duration::from_secs(30),
        )
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
    fn a_herdr_launch_initializes_stages_only_for_the_capture_handoff() {
        let (_dir, store, token) = fixture();
        initialize_for_launch(&store, &token, TerminalKind::Herdr).unwrap();
        let observed = observe(&store, super::super::read(&store).unwrap().as_ref());
        // Only the macOS host captures a shell; the Linux bash and Windows PowerShell
        // handoffs record no stages.
        #[cfg(not(any(target_os = "linux", windows)))]
        assert!(matches!(observed, HostObservation::Recorded(host)
            if host.terminal == TerminalKind::Herdr && host.stage == Stage::AwaitingHost));
        #[cfg(any(target_os = "linux", windows))]
        assert!(matches!(observed, HostObservation::NotRecorded));
    }

    #[test]
    fn admission_forward_progress_and_failure_are_immutable() {
        let (_dir, store, token) = fixture();
        assert!(matches!(
            observe(&store, None),
            HostObservation::NotRecorded
        ));
        initialize(&store, &token, TerminalKind::Iterm2).unwrap();
        assert!(initialize(&store, &token, TerminalKind::Iterm2).is_err());
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
            None,
        )
        .unwrap();
        assert_eq!(read(&store).stage, Stage::LaunchShellStarting);
        assert_eq!(read(&store).capture_elapsed_ms, Some(40));
        assert_eq!(read(&store).failure, Some(failure.clone()));
        assert!(fail(&store, &token, Some(identity()), failure, 55, None, None).is_err());
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
            initialize(&store, &token, TerminalKind::WezTerm).unwrap();
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
                    None,
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
        initialize(&store, &token, TerminalKind::Iterm2).unwrap();
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
        initialize(&store, &token, TerminalKind::Iterm2).unwrap();
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
        initialize(&store, &token, TerminalKind::Iterm2).unwrap();
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
    fn eligibility_reports_the_first_failing_fact_in_one_order() {
        use super::super::test_support::{Change::*, Expect, apply};
        const NOT: Expect = Expect::Text("launch host is no longer eligible");
        let cases = [
            ("pending", vec![], true, Expect::Pass),
            (
                "receipt missing",
                vec![ReceiptMissing],
                true,
                Expect::Text("missing launch receipt"),
            ),
            (
                "receipt damaged",
                vec![ReceiptDamaged],
                true,
                Expect::Receipt,
            ),
            (
                "receipt before status",
                vec![ReceiptDamaged, StatusDamaged],
                true,
                Expect::Receipt,
            ),
            ("receipt token", vec![ReceiptToken], true, NOT),
            ("spawning", vec![ReceiptSpawning], true, NOT),
            ("expired", vec![ReceiptExpired], true, NOT),
            (
                "expired without time",
                vec![ReceiptExpired],
                false,
                Expect::Pass,
            ),
            ("status closed", vec![StatusClosed], true, NOT),
            ("status failed", vec![StatusFailed], true, NOT),
            ("status damaged", vec![StatusDamaged], true, Expect::Status),
            ("tombstone", vec![Tombstone], true, NOT),
            (
                "tombstone damaged",
                vec![TombstoneDamaged],
                true,
                Expect::Tombstone,
            ),
            (
                "tombstone directory",
                vec![TombstoneDirectory],
                true,
                Expect::Tombstone,
            ),
            ("claim absent", vec![ClaimAbsent], true, NOT),
            ("claim other", vec![ClaimOther], true, NOT),
            (
                "claim unreadable",
                vec![ClaimUnreadable],
                true,
                Expect::Claim,
            ),
            // The status and the tombstone are read before the receipt is judged.
            (
                "damaged status with spawning",
                vec![ReceiptSpawning, StatusDamaged],
                true,
                Expect::Status,
            ),
            (
                "damaged status with receipt token",
                vec![ReceiptToken, StatusDamaged],
                true,
                Expect::Status,
            ),
            (
                "damaged status with expiry",
                vec![ReceiptExpired, StatusDamaged],
                true,
                Expect::Status,
            ),
            (
                "damaged status with expiry without time",
                vec![ReceiptExpired, StatusDamaged],
                false,
                Expect::Status,
            ),
            (
                "damaged tombstone with spawning",
                vec![ReceiptSpawning, TombstoneDamaged],
                true,
                Expect::Tombstone,
            ),
            (
                "damaged tombstone with expiry",
                vec![ReceiptExpired, TombstoneDamaged],
                true,
                Expect::Tombstone,
            ),
            (
                "status before damaged tombstone",
                vec![StatusClosed, TombstoneDamaged],
                true,
                NOT,
            ),
            (
                "phase before unreadable claim",
                vec![ReceiptSpawning, ClaimUnreadable],
                true,
                NOT,
            ),
            (
                "status before unreadable claim",
                vec![StatusClosed, ClaimUnreadable],
                true,
                NOT,
            ),
            (
                "tombstone before unreadable claim",
                vec![Tombstone, ClaimUnreadable],
                true,
                NOT,
            ),
            (
                "expiry before unreadable claim",
                vec![ReceiptExpired, ClaimUnreadable],
                true,
                NOT,
            ),
            (
                "unreadable claim without time",
                vec![ReceiptExpired, ClaimUnreadable],
                false,
                Expect::Claim,
            ),
        ];
        let mut failures = Vec::new();
        for (case, changes, require_time, expect) in cases {
            let (_dir, store, token) = fixture();
            apply(&store, &changes);
            let expected = expect.error(&store);
            let actual = eligible(&store, &token, require_time)
                .err()
                .map(|error| format!("{error:#}"));
            if actual != expected {
                failures.push(format!("{case}: {actual:?} != {expected:?}"));
            }
        }
        // The host record is judged only for a pending launch.
        for (case, initialized, changes, expected) in [
            (
                "not recorded",
                false,
                vec![],
                Some("launch host record is unavailable or terminal"),
            ),
            ("recorded", true, vec![], None),
            (
                "pending before record",
                false,
                vec![ReceiptSpawning],
                Some("launch host is no longer eligible"),
            ),
        ] {
            let (_dir, store, token) = fixture();
            if initialized {
                initialize(&store, &token, TerminalKind::Iterm2).unwrap();
            }
            apply(&store, &changes);
            let actual = current(&store, &token, true)
                .err()
                .map(|error| format!("{error:#}"));
            if actual.as_deref() != expected {
                failures.push(format!("current {case}: {actual:?} != {expected:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
    #[test]
    fn concurrent_admission_has_exactly_one_winner() {
        let (_dir, store, token) = fixture();
        initialize(&store, &token, TerminalKind::Iterm2).unwrap();
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

    // The record names exactly the kinds whose registration declares a launch host, by
    // their wire names; the writer refuses any other kind.
    #[test]
    fn record_accepts_exactly_the_registered_launch_host_kinds() {
        assert_eq!(LAUNCH_HOST_NAMES, ["iterm2", "wezterm", "herdr"]);
        let (_dir, store, token) = fixture();
        initialize(&store, &token, TerminalKind::Iterm2).unwrap();
        let mut host = serde_json::to_value(read(&store)).unwrap();
        for kind in TerminalKind::ALL {
            let wire = serde_json::to_value(kind).unwrap();
            assert_eq!(wire, kind.as_str());
            host["terminal"] = wire;
            match record::launch_host(&serde_json::to_vec(&host).unwrap()) {
                Ok(HostObservation::Recorded(record)) => {
                    assert!(kind.has_launch_host_form(), "{kind:?}");
                    assert_eq!(record.terminal, kind);
                }
                Ok(other) => panic!("{kind:?}: {other:?}"),
                Err(error) => {
                    assert!(!kind.has_launch_host_form(), "{kind:?}");
                    let expected = format!(
                        "unknown variant `{}`, expected one of `iterm2`, `wezterm`, `herdr` at line 1 column ",
                        kind.as_str()
                    );
                    assert!(error.to_string().starts_with(&expected), "{error}");
                }
            }
        }
        let (_dir, store, token) = fixture();
        let error = initialize(&store, &token, TerminalKind::Ghostty).unwrap_err();
        assert!(
            format!("{error:#}").starts_with(
                "unknown variant `ghostty`, expected one of `iterm2`, `wezterm`, `herdr` at line 1 column "
            ),
            "{error:#}"
        );
        assert!(
            store
                .record(CoreRecord::LaunchHost)
                .bytes()
                .unwrap()
                .is_none()
        );
    }

    // The writer's bytes name each launch host terminal by its wire word.
    #[test]
    fn host_record_bytes_name_each_launch_host_terminal() {
        for (terminal, word) in [
            (TerminalKind::Iterm2, "iterm2"),
            (TerminalKind::WezTerm, "wezterm"),
            (TerminalKind::Herdr, "herdr"),
        ] {
            let (_dir, store, token) = fixture();
            initialize(&store, &token, terminal).unwrap();
            let bytes = store
                .record(CoreRecord::LaunchHost)
                .bytes()
                .unwrap()
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let expected = format!(
                "{{\n  \"schema\": 1,\n  \"managed_session_id\": \"{}\",\n  \"claim_token\": \"{token}\",\n  \"terminal\": \"{word}\",\n  \"stage\": \"awaiting_host\",\n  \"host\": null,\n  \"updated_unix_ms\": {},\n  \"elapsed_ms\": 0,\n  \"capture_budget_ms\": 0,\n  \"capture_elapsed_ms\": null,\n  \"variables\": null,\n  \"background_children_stopped\": null,\n  \"failure\": null\n}}",
                store.directory().file_name().unwrap().to_str().unwrap(),
                value["updated_unix_ms"],
            );
            assert_eq!(String::from_utf8(bytes).unwrap(), expected);
            assert_eq!(read(&store).terminal, terminal);
        }
    }

    // A terminal without a launch host, or no terminal at all, is refused with serde's
    // own wording, which `observe` and doctor report unchanged.
    #[test]
    fn decoder_refuses_a_terminal_without_a_launch_host_in_serde_wording() {
        let (_dir, store, token) = fixture();
        initialize(&store, &token, TerminalKind::Iterm2).unwrap();
        let text = String::from_utf8(
            store
                .record(CoreRecord::LaunchHost)
                .bytes()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let line = "\"terminal\": \"iterm2\"";
        assert_eq!(text.matches(line).count(), 1);
        let outcome = |value: &str| {
            let bytes = text.replace(line, &format!("\"terminal\": {value}"));
            let outcome = match record::launch_host(bytes.as_bytes()) {
                Ok(HostObservation::Recorded(host)) => {
                    format!(
                        "recorded {}",
                        serde_json::to_string(&host.terminal).unwrap()
                    )
                }
                Ok(other) => format!("{other:?}"),
                Err(error) => error.to_string(),
            };
            format!("{value} => {outcome}")
        };
        let actual: Vec<_> = [
            r#""iterm2""#,
            r#""wezterm""#,
            r#""herdr""#,
            r#""ghostty""#,
            r#""kitty""#,
            r#""ITERM2""#,
            r#""WezTerm""#,
            r#""apple-terminal""#,
            r#""warp""#,
            r#""windows-console""#,
            r#""""#,
            "1",
            "null",
            "true",
            r#"["iterm2"]"#,
            r#"{"iterm2": null}"#,
            r#"{"wezterm": null}"#,
            r#"{"ghostty": null}"#,
            r#"{"iterm2": 1}"#,
            "{}",
            r#"{"iterm2": null, "wezterm": null}"#,
            r#""ghostty"  "#,
        ]
        .into_iter()
        .map(outcome)
        .collect();
        let expected = [
            r#""iterm2" => recorded "iterm2""#,
            r#""wezterm" => recorded "wezterm""#,
            r#""herdr" => recorded "herdr""#,
            r#""ghostty" => unknown variant `ghostty`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 23"#,
            r#""kitty" => unknown variant `kitty`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 21"#,
            r#""ITERM2" => unknown variant `ITERM2`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 22"#,
            r#""WezTerm" => unknown variant `WezTerm`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 23"#,
            r#""apple-terminal" => unknown variant `apple-terminal`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 30"#,
            r#""warp" => unknown variant `warp`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 20"#,
            r#""windows-console" => unknown variant `windows-console`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 31"#,
            r#""" => unknown variant ``, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 16"#,
            "1 => expected value at line 5 column 15",
            "null => expected value at line 5 column 15",
            "true => expected value at line 5 column 15",
            r#"["iterm2"] => expected value at line 5 column 15"#,
            r#"{"iterm2": null} => recorded "iterm2""#,
            r#"{"wezterm": null} => recorded "wezterm""#,
            r#"{"ghostty": null} => unknown variant `ghostty`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 24"#,
            r#"{"iterm2": 1} => invalid type: integer `1`, expected unit at line 5 column 26"#,
            "{} => expected value at line 5 column 16",
            r#"{"iterm2": null, "wezterm": null} => expected value at line 5 column 29"#,
            r#""ghostty"   => unknown variant `ghostty`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 23"#,
        ];
        assert_eq!(actual, expected);
        std::fs::write(
            store.record(CoreRecord::LaunchHost).path(),
            text.replace(line, "\"terminal\": \"ghostty\""),
        )
        .unwrap();
        match observe(&store, super::super::read(&store).unwrap().as_ref()) {
            HostObservation::Unreadable(reason) => assert_eq!(
                reason,
                "unknown variant `ghostty`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 23"
            ),
            other => panic!("{other:?}"),
        }
    }

    // The timeline's launch host entry names the terminal by the record's wire word, and
    // reports a terminal without a launch host as the decoder's text.
    #[test]
    fn timeline_names_each_launch_host_terminal_and_reports_others_unreadable() {
        let mut actual = Vec::new();
        for case in ["iterm2", "wezterm", "ghostty"] {
            let (dir, store, token) = fixture();
            crate::native::session::test_support::write_test_manifest(dir.path(), "codex");
            initialize(
                &store,
                &token,
                if case == "wezterm" {
                    TerminalKind::WezTerm
                } else {
                    TerminalKind::Iterm2
                },
            )
            .unwrap();
            if case == "ghostty" {
                let path = store.record(CoreRecord::LaunchHost).path().to_owned();
                let text = std::fs::read_to_string(&path).unwrap();
                std::fs::write(
                    &path,
                    text.replace("\"terminal\": \"iterm2\"", "\"terminal\": \"ghostty\""),
                )
                .unwrap();
            }
            let id = dir.path().file_name().unwrap().to_str().unwrap();
            let timeline = crate::native::query::timeline_value(&store, id, None).unwrap();
            let mut entry = None;
            let mut stack = vec![&timeline];
            while let Some(value) = stack.pop() {
                match value {
                    serde_json::Value::Object(map) => {
                        if map.get("stage") == Some(&serde_json::json!("launch_host")) {
                            entry = Some(value.clone());
                        }
                        stack.extend(map.values());
                    }
                    serde_json::Value::Array(values) => stack.extend(values),
                    _ => {}
                }
            }
            let entry = entry.unwrap();
            let detail = &entry["detail"];
            actual.push(format!(
                "{case}: {} {}",
                entry["record_state"],
                if detail.is_object() {
                    detail["terminal"].clone()
                } else {
                    detail.clone()
                }
            ));
        }
        let expected = [
            r#"iterm2: "observed" "iterm2""#,
            r#"wezterm: "observed" "wezterm""#,
            r#"ghostty: "unreadable" "unknown variant `ghostty`, expected one of `iterm2`, `wezterm`, `herdr` at line 5 column 23""#,
        ];
        assert_eq!(actual, expected);
    }

    // Every reason has one responsibility class; status.error and doctor map only the class.
    #[test]
    fn every_failure_reason_has_one_responsibility_class() {
        use FailureReason::*;
        use Responsibility::*;
        let table = [
            (IdentityUnavailable, SurfaceIdentity),
            (BindingMismatch, SurfaceIdentity),
            (BindingTimeout, Deadline),
            (LaunchBudgetExhausted, Deadline),
            (CaptureSpawnFailed, HostStep),
            (CaptureTimeout, StartupFiles),
            (CaptureExitNonzero, StartupFiles),
            (CaptureSignaled, StartupFiles),
            (CaptureNoOutput, StartupFiles),
            (CaptureOversized, StartupFiles),
            (CaptureInvalid, StartupFiles),
            (CleanupFailed, HostStep),
            (InputFlushFailed, HostStep),
            (LaunchShellExecFailed, HostStep),
        ];
        // Adding a reason fails to compile here until the table above names it.
        for (reason, _) in table {
            match reason {
                IdentityUnavailable
                | BindingMismatch
                | BindingTimeout
                | LaunchBudgetExhausted
                | CaptureSpawnFailed
                | CaptureTimeout
                | CaptureExitNonzero
                | CaptureSignaled
                | CaptureNoOutput
                | CaptureOversized
                | CaptureInvalid
                | CleanupFailed
                | InputFlushFailed
                | LaunchShellExecFailed => {}
            }
        }
        let actual: Vec<_> = table
            .iter()
            .map(|(reason, _)| (*reason, reason.responsibility()))
            .collect();
        assert_eq!(actual, table);
    }

    // Only a capture timeout adds a detail to its reason: the budget its record holds.
    #[test]
    fn only_a_capture_timeout_has_a_failure_detail() {
        let (_dir, store, token) = fixture();
        initialize(&store, &token, TerminalKind::Iterm2).unwrap();
        let mut host = read(&store);
        host.capture_budget_ms = 9996;
        assert_eq!(host.failure_detail(), None);
        for (reason, expected) in [
            (
                FailureReason::CaptureTimeout,
                Some("startup files did not finish within 9996 ms"),
            ),
            (FailureReason::CaptureExitNonzero, None),
            (FailureReason::LaunchBudgetExhausted, None),
        ] {
            host.failure = Some(Failure {
                reason,
                exit_code: None,
                signal: None,
            });
            assert_eq!(host.failure_detail().as_deref(), expected, "{reason:?}");
        }
    }

    // The timeline's launch host entry of a failed record: the record's update time is the
    // entry's time, and the detail holds every other field the doctor evidence holds.
    #[test]
    fn timeline_entry_of_a_failed_record_carries_its_time_and_fields() {
        let (dir, store, token) = fixture();
        crate::native::session::test_support::write_test_manifest(dir.path(), "codex");
        initialize(&store, &token, TerminalKind::WezTerm).unwrap();
        admit(&store, &token, identity()).unwrap();
        advance(
            &store,
            &token,
            identity(),
            Stage::EnvironmentPreparing,
            Progress {
                elapsed_ms: 3,
                capture_budget_ms: 9996,
                ..Progress::default()
            },
        )
        .unwrap();
        fail(
            &store,
            &token,
            Some(identity()),
            Failure {
                reason: FailureReason::CaptureExitNonzero,
                exit_code: Some(3),
                signal: None,
            },
            44,
            Some(40),
            None,
        )
        .unwrap();
        let mut host = read(&store);
        host.updated_unix_ms = 1_700_000_000_123;
        host.variables = Some(70);
        host.background_children_stopped = Some(1);
        store
            .record(CoreRecord::LaunchHost)
            .write_json(&host)
            .unwrap();
        let id = dir.path().file_name().unwrap().to_str().unwrap();
        let timeline = crate::native::query::timeline_value(&store, id, None).unwrap();
        let mut entries = Vec::new();
        let mut stack = vec![&timeline];
        while let Some(value) = stack.pop() {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("stage") == Some(&serde_json::json!("launch_host")) {
                        entries.push(value.clone());
                    }
                    stack.extend(map.values());
                }
                serde_json::Value::Array(values) => stack.extend(values),
                _ => {}
            }
        }
        assert_eq!(entries.len(), 1, "{entries:?}");
        let entry = &entries[0];
        assert_eq!(
            entry.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "detail",
                "event_id",
                "observed_unix_ms",
                "record_state",
                "request_id",
                "session",
                "source",
                "stage"
            ]
        );
        assert_eq!(entry["session"], id);
        assert_eq!(entry["observed_unix_ms"], 1_700_000_000_123u64);
        assert_eq!(entry["record_state"], "observed");
        assert_eq!(entry["source"], "launch-host.json");
        assert_eq!(
            entry["detail"].to_string(),
            r#"{"background_children_stopped":1,"capture_budget_ms":9996,"capture_elapsed_ms":40,"elapsed_ms":44,"failure":{"exit_code":3,"reason":"capture_exit_nonzero","signal":null},"stage":"environment_preparing","terminal":"wezterm","variables":70}"#
        );
    }
}
