use super::*;
use crate::native::clock::FakeClock;
use crate::native::session::Store;
use crate::native::session::owner::{ControlAttestation, OwnerAttestation};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Checkpoint {
    /// After an Origin's status and owner were read, before the consistency recheck.
    OriginRead,
    /// After a scanned session matched, before its recheck.
    Matched,
    /// After the scan admitted an entry, before any of its records were read.
    Entry,
}

type CheckpointHook = Box<dyn FnMut(Checkpoint, &Path)>;
type AttestHook = Box<dyn Fn(&NativeSessionOwner) -> OwnerAttestation>;

thread_local! {
    static RECEIPT_READS: Cell<usize> = const { Cell::new(0) };
    static CHECKPOINT: RefCell<Option<CheckpointHook>> = const { RefCell::new(None) };
}

pub(super) fn count_receipt_read() {
    RECEIPT_READS.with(|count| count.set(count.get() + 1));
}

pub(super) fn checkpoint(point: Checkpoint, directory: &Path) {
    CHECKPOINT.with(|cell| {
        if let Some(hook) = cell.borrow_mut().as_mut() {
            hook(point, directory);
        }
    });
}

/// Resets every observation hook of this thread when a test ends, also on panic.
struct Hooks;
impl Hooks {
    fn new() -> Self {
        Self::reset();
        Self
    }
    fn reset() {
        RECEIPT_READS.with(|count| count.set(0));
        CHECKPOINT.with(|cell| *cell.borrow_mut() = None);
    }
    fn checkpoint(&self, hook: impl FnMut(Checkpoint, &Path) + 'static) {
        CHECKPOINT.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
    }
    fn receipt_reads(&self) -> usize {
        RECEIPT_READS.with(Cell::get)
    }
}
impl Drop for Hooks {
    fn drop(&mut self) {
        Self::reset();
    }
}

/// A test's Origin environment: an attester the test sets (the platform observer until
/// it does), and a fake clock seeded from one instant that every deadline derives from.
struct Environment {
    attest: Rc<RefCell<Option<AttestHook>>>,
    attester: Box<dyn Fn(&NativeSessionOwner) -> ProcessObservation>,
    clock: FakeClock,
}

impl Environment {
    fn new() -> Self {
        let attest: Rc<RefCell<Option<AttestHook>>> = Rc::new(RefCell::new(None));
        let slot = Rc::clone(&attest);
        Self {
            attest,
            attester: Box::new(move |owner| match slot.borrow().as_ref() {
                Some(hook) => hook(owner).process,
                None => attest_through_platform_observer(owner),
            }),
            clock: FakeClock::new(Instant::now()),
        }
    }
    fn attest(&self, hook: impl Fn(&NativeSessionOwner) -> OwnerAttestation + 'static) {
        *self.attest.borrow_mut() = Some(Box::new(hook));
    }
    fn now(&mut self) -> Instant {
        self.clock.now()
    }
    fn origin(&mut self) -> OriginEnvironment<'_> {
        OriginEnvironment {
            attest: &*self.attester,
            clock: &mut self.clock,
        }
    }
}

fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

/// A session fixture written the way the records are named by `CoreRecord`.
struct Session {
    directory: PathBuf,
}

impl Session {
    fn new(root: &Path, id: &str, state: &str) -> Self {
        let directory = root.join(id);
        fs::create_dir_all(directory.join(CoreRecord::Events.name())).unwrap();
        write(
            &directory.join(CoreRecord::Manifest.name()),
            &json!({"schema": 1, "id": id, "provider": "codex", "provider_path": "codex",
                "provider_version": "0.162.1", "workspace": root, "title": "origin fixture",
                "yolo": false, "created_unix_ms": 1}),
        );
        let session = Self { directory };
        session.status(state, 3);
        session
    }
    fn record(&self, record: CoreRecord) -> PathBuf {
        self.directory.join(record.name())
    }
    fn status(&self, state: &str, generation: u64) {
        write(
            &self.record(CoreRecord::Status),
            &json!({"state": state, "generation": generation, "updated_unix_ms": 1,
                "exit_code": null, "error": null}),
        );
    }
    fn tombstone(&self) {
        write(
            &self.record(CoreRecord::Closed),
            &json!({"state": "closed", "generation": 9, "updated_unix_ms": 1,
                "exit_code": null, "error": null}),
        );
    }
    fn launch(&self, token: &str) {
        write(
            &self.record(CoreRecord::Launch),
            &json!({"schema": 1, "claim_token": token, "deadline_unix_ms": 1, "phase": "spawned"}),
        );
    }
    fn receipt(&self, token: &str, created: u128, source: Option<&str>) {
        let requests = self.record(CoreRecord::Requests);
        fs::create_dir_all(&requests).unwrap();
        let mut receipt = json!({"schema": 1, "request_id": format!("request-{token}"),
            "claim_token": token, "event_file": format!("event-{token}.json"),
            "created_unix_ms": created, "context_sources": []});
        if let Some(source) = source {
            receipt["source"] = json!(source);
        }
        write(&requests.join(format!("{token}.json")), &receipt);
    }
    fn damaged_receipt(&self, token: &str) {
        let requests = self.record(CoreRecord::Requests);
        fs::create_dir_all(&requests).unwrap();
        fs::write(requests.join(format!("{token}.json")), b"{").unwrap();
    }
    /// A launched session whose initial request was sent from `source`.
    fn started_from(root: &Path, id: &str, state: &str, source: &str) -> Self {
        let session = Self::new(root, id, state);
        session.launch("100-1");
        session.receipt("100-1", 100, Some(source));
        session
    }
    fn owner(&self, managed: Option<&str>, pid: u32) {
        let mut owner = json!({"pid": pid});
        if let Some(managed) = managed {
            owner["managed_session_id"] = json!(managed);
        }
        write(&self.record(CoreRecord::Owner), &owner);
    }
    fn reader(&self) -> Reader {
        Reader::open_unchecked(&self.directory)
    }
}

fn read(environment: &mut Environment, session: &Session) -> OriginReference {
    let mut budget = ReadBudget::for_query(None, &mut environment.clock);
    read_reference(&session.reader(), &mut budget, &mut environment.clock)
}

fn observed(environment: &mut Environment, root: &Path, session: &Session) -> OriginObservation {
    let mut environment = environment.origin();
    let mut budget = ReadBudget::for_query(None, environment.clock);
    let reference = read_reference(&session.reader(), &mut budget, environment.clock);
    observe(root, reference, &mut budget, &mut environment)
}

fn attestation(process: ProcessObservation, control: ControlAttestation) -> OwnerAttestation {
    OwnerAttestation {
        process,
        control,
        pid_alive: Some(true),
    }
}

#[test]
fn the_launch_claim_names_the_initial_request_whatever_the_timestamps_say() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let session = Session::new(root.path(), "session-child", "ready");
    session.launch("200-1");
    session.receipt("200-1", 500, Some("session-origin"));
    // Follow-ups from elsewhere, older and equal in wall-clock time.
    session.receipt("100-1", 10, Some("session-other"));
    session.receipt("300-1", 500, Some("external"));
    let reference = read(&mut environment, &session);
    assert_eq!(reference.kind, SourceKind::Managed);
    assert_eq!(reference.id.as_deref(), Some("session-origin"));
    assert_eq!(reference.request_id.as_deref(), Some("request-200-1"));
    // The Snapshot selector applies the same rule.
    let snapshot = session.reader().snapshot().unwrap();
    assert_eq!(super::reference(&snapshot), reference);
}

#[test]
fn a_missing_or_damaged_initial_receipt_never_promotes_a_follow_up() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let session = Session::new(root.path(), "session-child", "ready");
    session.launch("200-1");
    session.receipt("300-1", 1, Some("session-other"));
    let missing = read(&mut environment, &session);
    assert_eq!(missing.kind, SourceKind::Unknown);
    assert_eq!(missing.id, None);
    assert_eq!(missing.reason, "initial_request_unavailable");
    assert_eq!(
        super::reference(&session.reader().snapshot().unwrap()).reason,
        "initial_request_unavailable"
    );

    session.damaged_receipt("200-1");
    let damaged = read(&mut environment, &session);
    assert_eq!(damaged.kind, SourceKind::Unknown);
    assert_eq!(damaged.reason, "initial_receipt_unreadable");
    let snapshot = super::reference(&session.reader().snapshot().unwrap());
    assert_eq!(snapshot.kind, SourceKind::Unknown);
    assert_eq!(snapshot.reason, "initial_receipt_unreadable");
}

#[test]
fn an_unrelated_unreadable_receipt_does_not_hide_a_readable_initial_one() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let session = Session::started_from(root.path(), "session-child", "ready", "session-origin");
    session.damaged_receipt("300-1");
    assert_eq!(
        read(&mut environment, &session).id.as_deref(),
        Some("session-origin")
    );
    let snapshot = session.reader().snapshot().unwrap();
    assert_eq!(snapshot.unreadable_requests, 1);
    assert_eq!(
        super::reference(&snapshot).id.as_deref(),
        Some("session-origin")
    );
}

#[test]
fn legacy_external_and_sourceless_initial_requests_are_distinct_evidence() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    // Before 0.0.7: no launch record and no receipts.
    let legacy = Session::new(root.path(), "session-legacy", "ready");
    let reference = read(&mut environment, &legacy);
    assert_eq!(
        (reference.kind, reference.reason.as_str()),
        (SourceKind::Unknown, "initial_request_unavailable")
    );
    // A launch whose receipt carries no source is not external.
    let sourceless = Session::new(root.path(), "session-sourceless", "ready");
    sourceless.launch("1-1");
    sourceless.receipt("1-1", 1, None);
    let reference = read(&mut environment, &sourceless);
    assert_eq!(
        (reference.kind, reference.reason.as_str()),
        (SourceKind::Unknown, "source_unavailable")
    );
    assert_eq!(reference.request_id.as_deref(), Some("request-1-1"));
    let external = Session::started_from(root.path(), "session-external", "ready", "external");
    let observation = observed(&mut environment, root.path(), &external);
    assert_eq!(observation.reference.kind, SourceKind::External);
    assert_eq!(observation.reference.id, None);
    assert_eq!(observation.ended, None);
    assert_eq!(observation.reason, "origin_external");
    assert_eq!(
        observation.value(),
        json!({"kind": "external", "request_id": "request-100-1", "stored_state": null,
            "ended": null, "reason": "origin_external", "detail": null, "derived_from": "records"})
    );
    // A receipt written before its launch record is not yet evidence.
    let launching = Session::new(root.path(), "session-launching", "launching");
    launching.receipt("100-1", 1, Some("session-origin"));
    assert_eq!(
        read(&mut environment, &launching).reason,
        "initial_request_unavailable"
    );
}

#[test]
fn invalid_and_self_sources_are_unknown_and_never_joined_to_a_path() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    for (id, source) in [
        ("session-a", "../session-escape"),
        ("session-b", ""),
        ("session-c", "session-c"),
        ("session-d", "request-1"),
    ] {
        let session = Session::started_from(root.path(), id, "ready", source);
        let observation = observed(&mut environment, root.path(), &session);
        assert_eq!(observation.reference.kind, SourceKind::Unknown, "{source}");
        assert_eq!(observation.reference.id, None);
        assert_eq!(observation.reason, "invalid_source");
        assert_eq!(observation.ended, None);
    }
}

#[test]
fn a_cycle_reads_one_origin_and_a_foreign_or_linked_origin_is_not_followed() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let a = Session::started_from(root.path(), "session-a", "ready", "session-b");
    let b = Session::started_from(root.path(), "session-b", "closed", "session-a");
    b.tombstone();
    let observation = observed(&mut environment, root.path(), &a);
    assert_eq!(observation.reason, "recorded_closed");
    assert_eq!(observation.ended, Some(true));
    let observation = observed(&mut environment, root.path(), &b);
    assert_eq!(observation.reference.id.as_deref(), Some("session-a"));
    assert_eq!(observation.reason, "owner_unverified");

    // An id from another state root is absent here, and absence is not an end.
    let other = tempfile::tempdir().unwrap();
    Session::new(other.path(), "session-elsewhere", "closed").tombstone();
    let c = Session::started_from(root.path(), "session-c", "ready", "session-elsewhere");
    let observation = observed(&mut environment, root.path(), &c);
    assert_eq!(
        (observation.ended, observation.reason.as_str()),
        (None, "origin_missing")
    );
    assert_eq!(
        observation.reference.id.as_deref(),
        Some("session-elsewhere")
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(
            other.path().join("session-elsewhere"),
            root.path().join("session-elsewhere"),
        )
        .unwrap();
        let observation = observed(&mut environment, root.path(), &c);
        assert_eq!(
            (observation.ended, observation.reason.as_str()),
            (None, "origin_unreadable")
        );
        fs::remove_file(root.path().join("session-elsewhere")).unwrap();
        // A linked record inside a real Origin directory is refused, not followed.
        let d = Session::new(root.path(), "session-elsewhere", "ready");
        fs::remove_file(d.record(CoreRecord::Status)).unwrap();
        symlink(
            other
                .path()
                .join("session-elsewhere")
                .join(CoreRecord::Status.name()),
            d.record(CoreRecord::Status),
        )
        .unwrap();
        let observation = observed(&mut environment, root.path(), &c);
        assert_eq!(observation.reason, "origin_unreadable");
        // A linked requests directory supplies no initial receipt.
        let e = Session::new(root.path(), "session-e", "ready");
        e.launch("100-1");
        let outside = tempfile::tempdir().unwrap();
        write(
            &outside.path().join("100-1.json"),
            &json!({"schema": 1, "request_id": "request-x", "claim_token": "100-1",
                "event_file": "event-x.json", "source": "session-a"}),
        );
        symlink(outside.path(), e.record(CoreRecord::Requests)).unwrap();
        assert_eq!(
            read(&mut environment, &e).reason,
            "initial_receipt_unreadable"
        );
    }
}

#[test]
fn a_reopened_session_has_its_reopen_caller_as_origin_not_its_conversation_source() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let source = Session::started_from(root.path(), "session-source", "closed", "session-old");
    source.tombstone();
    let reopened = Session::started_from(root.path(), "session-reopened", "ready", "external");
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(reopened.record(CoreRecord::Manifest)).unwrap()).unwrap();
    manifest["resumed_from"] = json!({"session": "session-source",
        "provider_session_id": "thread", "event_id": "event-1.json"});
    write(&reopened.record(CoreRecord::Manifest), &manifest);
    let observation = observed(&mut environment, root.path(), &reopened);
    assert_eq!(observation.reference.kind, SourceKind::External);
    assert_eq!(observation.ended, None);
}

#[test]
fn recorded_lifecycle_ends_and_the_attested_owner_decide_ended() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let child = Session::started_from(root.path(), "session-child", "ready", "session-origin");
    let origin = Session::new(root.path(), "session-origin", "ready");
    for (state, reason) in [
        ("closed", "recorded_closed"),
        ("failed", "recorded_failed"),
        ("exited", "recorded_exited"),
    ] {
        origin.status(state, 4);
        let observation = observed(&mut environment, root.path(), &child);
        assert_eq!(observation.ended, Some(true), "{state}");
        assert_eq!(observation.reason, reason);
        assert_eq!(observation.stored_state.as_deref(), Some(state));
    }
    origin.status("future-state", 4);
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(
        (observation.ended, observation.reason.as_str()),
        (None, "origin_unverified")
    );
    assert_eq!(observation.stored_state.as_deref(), Some("future-state"));

    let _hooks = Hooks::new();
    type Case = (
        &'static str,
        fn() -> OwnerAttestation,
        Option<bool>,
        &'static str,
    );
    let cases: [Case; 7] = [
        (
            "exited",
            || {
                attestation(
                    ProcessObservation::Gone(Gone::Exited),
                    ControlAttestation::NotRecorded,
                )
            },
            Some(true),
            "owner_exited",
        ),
        (
            "reused",
            || {
                attestation(
                    ProcessObservation::Gone(Gone::Reused("process 7 was reused".into())),
                    ControlAttestation::NotRecorded,
                )
            },
            Some(true),
            "owner_reused",
        ),
        (
            "incarnation",
            || {
                attestation(
                    ProcessObservation::Live(Attested::Incarnation),
                    ControlAttestation::Verified,
                )
            },
            Some(false),
            "owner_live",
        ),
        (
            "pid-only",
            || {
                attestation(
                    ProcessObservation::Live(Attested::PidOnly),
                    ControlAttestation::NotRecorded,
                )
            },
            Some(false),
            "owner_live",
        ),
        (
            "control changed",
            || {
                attestation(
                    ProcessObservation::Live(Attested::Incarnation),
                    ControlAttestation::Changed("owner TTY changed".into()),
                )
            },
            Some(false),
            "owner_live",
        ),
        (
            "control unknown",
            || {
                attestation(
                    ProcessObservation::Live(Attested::Incarnation),
                    ControlAttestation::Unknown("exited during attestation".into()),
                )
            },
            Some(false),
            "owner_live",
        ),
        (
            "process unknown with a dead-looking pid",
            || OwnerAttestation {
                process: ProcessObservation::Unknown("access denied".into()),
                control: ControlAttestation::NotRecorded,
                pid_alive: Some(false),
            },
            None,
            "owner_unverified",
        ),
    ];
    origin.owner(Some("session-origin"), 7);
    for state in [
        "launching",
        "running",
        "awaiting-initial-input",
        "ready",
        "claimed",
        "working",
        "resume-pending",
    ] {
        origin.status(state, 5);
        for (name, attestation, ended, reason) in cases {
            environment.attest(move |_| attestation());
            let observation = observed(&mut environment, root.path(), &child);
            assert_eq!(observation.ended, ended, "{state} {name}");
            assert_eq!(observation.reason, reason, "{state} {name}");
            assert_eq!(observation.stored_state.as_deref(), Some(state));
        }
    }
    // An owner that is not bound to the Origin is never attested.
    environment.attest(|_| panic!("an unbound owner must not be attested"));
    origin.status("ready", 6);
    for managed in [None, Some("session-other")] {
        origin.owner(managed, 7);
        let observation = observed(&mut environment, root.path(), &child);
        assert_eq!(
            (observation.ended, observation.reason.as_str()),
            (None, "owner_unverified")
        );
    }
    fs::remove_file(origin.record(CoreRecord::Owner)).unwrap();
    assert_eq!(
        observed(&mut environment, root.path(), &child).reason,
        "owner_unverified"
    );
    fs::remove_dir_all(&origin.directory).unwrap();
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(
        (observation.ended, observation.reason.as_str()),
        (None, "origin_missing")
    );
    assert_eq!(observation.reference.id.as_deref(), Some("session-origin"));
}

#[test]
fn a_live_pid_only_owner_is_live_through_the_platform_observer() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let child = Session::started_from(root.path(), "session-child", "ready", "session-origin");
    let origin = Session::new(root.path(), "session-origin", "ready");
    origin.owner(Some("session-origin"), std::process::id());
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(
        (observation.ended, observation.reason.as_str()),
        (Some(false), "owner_live")
    );
    assert!(observation.detail.unwrap().contains("pid-only"));
}

#[test]
fn a_tombstone_ahead_of_its_status_is_a_recorded_close() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let child = Session::started_from(root.path(), "session-child", "ready", "session-origin");
    let origin = Session::new(root.path(), "session-origin", "ready");
    // The close wrote its tombstone and stopped before status.json caught up.
    origin.tombstone();
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(observation.ended, Some(true));
    assert_eq!(observation.reason, "recorded_closed");
    assert_eq!(observation.stored_state.as_deref(), Some("ready"));
    assert!(observation.detail.unwrap().contains("still says ready"));
    // A tombstone with an unreadable status keeps its end and reports the gap.
    fs::write(origin.record(CoreRecord::Status), b"{").unwrap();
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(
        (observation.ended, observation.stored_state.clone()),
        (Some(true), None)
    );
    // A malformed tombstone is not evidence of anything.
    fs::write(origin.record(CoreRecord::Closed), b"{").unwrap();
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(
        (observation.ended, observation.reason.as_str()),
        (None, "origin_unreadable")
    );
}

#[test]
fn a_change_between_the_reads_is_busy_not_a_verdict() {
    let hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let child = Session::started_from(root.path(), "session-child", "ready", "session-origin");
    let origin = Session::new(root.path(), "session-origin", "ready");
    origin.owner(Some("session-origin"), 7);
    environment.attest(|_| {
        attestation(
            ProcessObservation::Gone(Gone::Exited),
            ControlAttestation::NotRecorded,
        )
    });
    // The generation moves while the owner is attested.
    let status = origin.record(CoreRecord::Status);
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::OriginRead {
            write(
                &status,
                &json!({"state": "working", "generation": 4, "updated_unix_ms": 2,
                    "exit_code": null, "error": null}),
            );
        }
    });
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(
        (observation.ended, observation.reason.as_str()),
        (None, "origin_busy")
    );
    // The close commits its tombstone between the reads.
    origin.status("ready", 3);
    let closed = origin.record(CoreRecord::Closed);
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::OriginRead && !closed.exists() {
            write(
                &closed,
                &json!({"state": "closed", "generation": 4, "updated_unix_ms": 2,
                    "exit_code": null, "error": null}),
            );
        }
    });
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(observation.reason, "origin_busy");
    // The next query sees the commit.
    assert_eq!(
        observed(&mut environment, root.path(), &child).reason,
        "recorded_closed"
    );
}

#[test]
fn budgets_and_damaged_records_leave_the_end_unknown() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let child = Session::started_from(root.path(), "session-child", "ready", "session-origin");
    let origin = Session::new(root.path(), "session-origin", "closed");
    let reference = read(&mut environment, &child);
    // No time left: nothing is read.
    let now = environment.now();
    let observation = observe(
        root.path(),
        reference.clone(),
        &mut ReadBudget::new(now, OBSERVATION_BYTES),
        &mut environment.origin(),
    );
    assert_eq!(observation.reason, "origin_budget_exhausted");
    // Too few bytes left for the manifest.
    let observation = observe(
        root.path(),
        reference.clone(),
        &mut ReadBudget::new(now + Duration::from_secs(5), 10),
        &mut environment.origin(),
    );
    assert_eq!(observation.reason, "origin_budget_exhausted");
    // An oversized or non-UTF-8 record is damaged.
    fs::write(
        origin.record(CoreRecord::Status),
        vec![b' '; RECORD_LIMIT as usize + 1],
    )
    .unwrap();
    assert_eq!(
        observed(&mut environment, root.path(), &child).reason,
        "origin_unreadable"
    );
    fs::write(origin.record(CoreRecord::Status), b"\xff\xfe").unwrap();
    assert_eq!(
        observed(&mut environment, root.path(), &child).reason,
        "origin_unreadable"
    );
    fs::remove_file(origin.record(CoreRecord::Manifest)).unwrap();
    assert_eq!(
        observed(&mut environment, root.path(), &child).reason,
        "origin_unreadable"
    );
    // The reference's own budget is bounded too.
    let mut empty = ReadBudget::new(environment.now(), OBSERVATION_BYTES);
    assert_eq!(
        read_reference(&child.reader(), &mut empty, &mut environment.clock).reason,
        "origin_budget_exhausted"
    );
}

#[test]
fn the_rendered_fields_match_the_public_shape() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let child = Session::started_from(root.path(), "session-child", "ready", "session-origin");
    Session::new(root.path(), "session-origin", "closed").tombstone();
    let mut value = json!({});
    observed(&mut environment, root.path(), &child).add_fields(&mut value);
    assert_eq!(
        value,
        json!({"origin": "session-origin", "origin_observation": {"kind": "managed",
            "request_id": "request-100-1", "stored_state": "closed", "ended": true,
            "reason": "recorded_closed", "detail": null, "derived_from": "records"}})
    );
    let legacy = Session::new(root.path(), "session-legacy", "ready");
    let mut value = json!({});
    observed(&mut environment, root.path(), &legacy).add_fields(&mut value);
    assert_eq!(
        value,
        json!({"origin": null, "origin_observation": {"kind": "unknown", "request_id": null,
            "stored_state": null, "ended": null, "reason": "initial_request_unavailable",
            "detail": "no launch record names the initial request", "derived_from": "records"}})
    );
}

#[test]
fn the_query_cache_reads_an_origin_once_per_response() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let a = Session::started_from(root.path(), "session-a", "ready", "session-origin");
    let b = Session::started_from(root.path(), "session-b", "ready", "session-origin");
    let origin = Session::new(root.path(), "session-origin", "ready");
    origin.owner(Some("session-origin"), 7);
    let attests = std::rc::Rc::new(Cell::new(0));
    let counted = attests.clone();
    environment.attest(move |_| {
        counted.set(counted.get() + 1);
        attestation(
            ProcessObservation::Gone(Gone::Exited),
            ControlAttestation::NotRecorded,
        )
    });
    let mut cache = ObservationCache::default();
    for session in [&a, &b] {
        let reference = read(&mut environment, session);
        let observation = cache.observe(root.path(), reference, None, &mut environment.origin());
        assert_eq!(observation.reason, "owner_exited");
        assert_eq!(
            observation.reference.request_id.as_deref(),
            Some("request-100-1")
        );
    }
    assert_eq!(attests.get(), 1);
}

/// Every path under `root` with its bytes and permissions.
fn tree(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>, u32)> {
    fn walk(path: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>, u32)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        #[cfg(unix)]
        let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions());
        #[cfg(not(unix))]
        let mode = u32::from(metadata.permissions().readonly());
        if metadata.is_dir() {
            out.push((path.to_owned(), None, mode));
            let mut entries: Vec<_> = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for entry in entries {
                walk(&entry, out);
            }
        } else {
            out.push((path.to_owned(), fs::read(path).ok(), mode));
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

#[test]
fn the_reader_and_the_scan_change_no_record_and_wait_on_no_lock() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let child = Session::started_from(root.path(), "session-child", "working", "session-origin");
    let origin = Session::new(root.path(), "session-origin", "ready");
    // A dead owner and a pending completion journal: what repair and recovery would act on.
    origin.owner(Some("session-origin"), 7);
    write(
        &origin.record(CoreRecord::Completion),
        &json!({"schema": 1, "claim_token": "100-1", "event_file": "event-1.json"}),
    );
    fs::write(origin.record(CoreRecord::TurnClaim), "100-1\n").unwrap();
    environment.attest(|_| {
        attestation(
            ProcessObservation::Gone(Gone::Exited),
            ControlAttestation::NotRecorded,
        )
    });
    // Both lifecycle locks are held by a writer for the whole observation.
    let _origin_lock = Store::open_unchecked(&origin.directory).lock().unwrap();
    let _child_lock = Store::open_unchecked(&child.directory).lock().unwrap();
    let before = tree(root.path());
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(observation.reason, "owner_exited");
    let deadline = environment.now() + SCAN_TIME;
    let scan = scan_started_from(
        root.path(),
        "session-origin",
        deadline,
        &mut environment.clock,
    );
    assert_eq!(scan.open, vec!["session-child".to_owned()]);
    assert_eq!(tree(root.path()), before);
}

fn open_children(environment: &mut Environment, root: &Path, origin: &str) -> StartedFrom {
    let deadline = environment.now() + Duration::from_secs(60);
    scan_started_from(root, origin, deadline, &mut environment.clock)
}

#[test]
fn the_scan_reports_unclosed_sessions_started_from_the_closed_one() {
    let hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    Session::new(root.path(), "session-origin", "closed").tombstone();
    Session::started_from(root.path(), "session-z", "ready", "session-origin");
    Session::started_from(root.path(), "session-b", "failed", "session-origin");
    Session::started_from(root.path(), "session-other", "ready", "session-else");
    Session::started_from(root.path(), "session-ext", "ready", "external");
    let closed = Session::started_from(root.path(), "session-closed", "closed", "session-origin");
    closed.tombstone();
    // A session that started from the origin but whose follow-up came from elsewhere.
    let followed = Session::started_from(root.path(), "session-y", "ready", "session-origin");
    followed.receipt("200-1", 1, Some("session-else"));
    let scan = open_children(&mut environment, root.path(), "session-origin");
    assert_eq!(scan.open, vec!["session-b", "session-y", "session-z"]);
    assert!(!scan.incomplete, "{:?}", scan.incomplete_reasons);
    assert_eq!(scan.scanned.sessions, 6);
    assert_eq!(scan.scanned.listed, 3);
    // Closed sessions are excluded before their receipts are read; each other one reads one.
    assert_eq!(hooks.receipt_reads(), 5);
    assert_eq!(
        serde_json::to_value(&scan).unwrap(),
        json!({"open": ["session-b", "session-y", "session-z"], "incomplete": false,
            "incomplete_reasons": [], "additional_incomplete_reasons": 0,
            "scanned": {"sessions": 6, "listed": 3}})
    );
}

#[test]
fn the_scan_never_counts_missing_evidence_as_unrelated() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    Session::new(root.path(), "session-legacy", "ready");
    Session::new(root.path(), "session-legacy-closed", "closed");
    let damaged = Session::new(root.path(), "session-damaged", "ready");
    damaged.launch("100-1");
    damaged.damaged_receipt("100-1");
    Session::new(root.path(), "session-future", "future-state");
    let closing = Session::started_from(root.path(), "session-closing", "ready", "session-origin");
    closing.tombstone();
    Session::started_from(root.path(), "session-bad", "ready", "../x");
    fs::create_dir(root.path().join("session-empty")).unwrap();
    let scan = open_children(&mut environment, root.path(), "session-origin");
    assert!(scan.open.is_empty());
    assert!(scan.incomplete);
    let mut reasons: Vec<_> = scan
        .incomplete_reasons
        .iter()
        .map(|r| {
            (
                r.session.clone().unwrap(),
                r.reason.split(':').next().unwrap().to_owned(),
            )
        })
        .collect();
    reasons.sort();
    assert_eq!(
        reasons,
        vec![
            ("session-bad".into(), "invalid_source".into()),
            ("session-closing".into(), "close_in_progress".into()),
            (
                "session-damaged".into(),
                "initial_receipt_unreadable".into()
            ),
            ("session-empty".into(), "records_unreadable".into()),
            ("session-future".into(), "records_unreadable".into()),
            (
                "session-legacy".into(),
                "initial_request_unavailable".into()
            ),
        ]
    );
}

#[test]
fn a_match_that_closes_or_changes_before_it_is_reported_is_not_open() {
    let hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let closes = Session::started_from(root.path(), "session-a", "ready", "session-origin");
    let changes = Session::started_from(root.path(), "session-b", "ready", "session-origin");
    let removed = Session::started_from(root.path(), "session-c", "ready", "session-origin");
    let (a, b, c) = (
        closes.directory.clone(),
        changes.directory.clone(),
        removed.directory.clone(),
    );
    hooks.checkpoint(move |point, directory| {
        if point != Checkpoint::Matched {
            return;
        }
        let status = directory.join(CoreRecord::Status.name());
        let closed = json!({"state": "closed", "generation": 4, "updated_unix_ms": 2,
            "exit_code": null, "error": null});
        if directory == a {
            write(&directory.join(CoreRecord::Closed.name()), &closed);
            write(&status, &closed);
        } else if directory == b {
            write(
                &status,
                &json!({"state": "claimed", "generation": 4, "updated_unix_ms": 2,
                    "exit_code": null, "error": null}),
            );
        } else if directory == c {
            fs::remove_file(&status).unwrap();
        }
    });
    let scan = open_children(&mut environment, root.path(), "session-origin");
    assert!(scan.open.is_empty());
    let mut reasons: Vec<_> = scan
        .incomplete_reasons
        .iter()
        .map(|r| (r.session.clone().unwrap(), r.reason.clone()))
        .collect();
    reasons.sort();
    assert_eq!(
        reasons,
        vec![
            ("session-b".into(), "session_changed".into()),
            ("session-c".into(), "session_changed".into()),
        ]
    );
}

#[test]
fn an_exhausted_budget_stops_the_scan_without_io_or_midway() {
    let hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    for index in 0..5 {
        Session::started_from(
            root.path(),
            &format!("session-{index}"),
            "ready",
            "session-origin",
        );
    }
    // No time left: nothing is read, not even the root.
    let deadline = environment.now();
    let scan = scan_started_from(
        &root.path().join("missing"),
        "session-origin",
        deadline,
        &mut environment.clock,
    );
    assert_eq!(scan.incomplete_reasons[0].reason, "time_budget_exhausted");
    assert_eq!(scan.scanned.sessions, 0);
    // The clock passes the deadline while the third entry is examined.
    let entries = Rc::new(Cell::new(0));
    let seen = entries.clone();
    let clock = environment.clock.shared();
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::Entry {
            seen.set(seen.get() + 1);
            if seen.get() == 3 {
                clock.set(clock.get() + Duration::from_secs(120));
            }
        }
    });
    let scan = open_children(&mut environment, root.path(), "session-origin");
    assert_eq!(scan.open.len(), 2);
    assert!(scan.incomplete);
    assert_eq!(scan.incomplete_reasons.len(), 1);
    assert_eq!(scan.incomplete_reasons[0].reason, "time_budget_exhausted");
    assert_eq!(scan.scanned.sessions, 3);
    assert_eq!(entries.get(), 3);
}
#[test]
fn the_scan_refuses_damaged_records_and_reports_root_and_entry_errors() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let oversized = Session::started_from(root.path(), "session-big", "ready", "session-origin");
    fs::write(
        oversized.record(CoreRecord::Launch),
        vec![b' '; RECORD_LIMIT as usize + 1],
    )
    .unwrap();
    let invalid = Session::new(root.path(), "session-utf", "ready");
    fs::write(invalid.record(CoreRecord::Status), b"\xff").unwrap();
    #[cfg(unix)]
    {
        let linked = Session::started_from(root.path(), "session-link", "ready", "session-origin");
        fs::remove_file(linked.record(CoreRecord::Status)).unwrap();
        std::os::unix::fs::symlink(
            oversized.record(CoreRecord::Status),
            linked.record(CoreRecord::Status),
        )
        .unwrap();
    }
    let scan = open_children(&mut environment, root.path(), "session-origin");
    assert!(scan.open.is_empty());
    let reasons: std::collections::BTreeMap<_, _> = scan
        .incomplete_reasons
        .iter()
        .map(|r| (r.session.clone().unwrap(), r.reason.clone()))
        .collect();
    assert!(reasons["session-big"].starts_with("initial_request_unavailable"));
    assert!(reasons["session-utf"].starts_with("records_unreadable"));
    #[cfg(unix)]
    assert!(reasons["session-link"].starts_with("records_unreadable"));

    let scan = open_children(
        &mut environment,
        &root.path().join("missing"),
        "session-origin",
    );
    assert!(
        scan.incomplete_reasons[0]
            .reason
            .starts_with("root_unreadable")
    );

    use crate::native::session::enumerate::{TestEntry, with_entries};
    let scan = with_entries(
        vec![
            Err(std::io::Error::other("entry")),
            Ok(TestEntry {
                id: "session-kind".into(),
                directory: root.path().join("session-kind"),
                directory_kind: Err(std::io::Error::other("kind")),
            }),
        ],
        || open_children(&mut environment, root.path(), "session-origin"),
    );
    let reasons: Vec<_> = scan
        .incomplete_reasons
        .iter()
        .map(|r| (r.session.clone(), r.reason.clone()))
        .collect();
    assert_eq!(
        reasons,
        vec![
            (None, "entry_unreadable: entry".to_owned()),
            (
                Some("session-kind".to_owned()),
                "entry_unreadable: kind".to_owned()
            ),
        ]
    );
}

#[test]
fn every_scan_cap_is_reported_and_the_reasons_are_capped() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    // Entries without records: each is examined and each is a reason.
    let entries = (0..SCAN_ENTRIES + 3)
        .map(|index| {
            Ok(crate::native::session::enumerate::TestEntry {
                id: format!("session-{index:05}"),
                directory: root.path().join(format!("session-{index:05}")),
                directory_kind: Ok(true),
            })
        })
        .collect();
    let scan = crate::native::session::enumerate::with_entries(entries, || {
        open_children(&mut environment, root.path(), "session-origin")
    });
    assert_eq!(scan.scanned.sessions, SCAN_ENTRIES);
    assert_eq!(scan.incomplete_reasons.len(), SCAN_REASONS);
    // Every examined entry and the entry limit itself.
    assert_eq!(
        scan.additional_incomplete_reasons,
        SCAN_ENTRIES + 1 - SCAN_REASONS as u64
    );

    for index in 0..=SCAN_OPEN {
        Session::started_from(
            root.path(),
            &format!("session-{index:04}"),
            "ready",
            "session-origin",
        );
    }
    let scan = open_children(&mut environment, root.path(), "session-origin");
    assert_eq!(scan.open.len(), SCAN_OPEN);
    let mut sorted = scan.open.clone();
    sorted.sort();
    assert_eq!(scan.open, sorted);
    assert!(scan.incomplete_reasons[0].reason.starts_with("open_limit"));
    assert_eq!(scan.scanned.listed, SCAN_OPEN as u64);
}

#[test]
fn the_byte_limit_stops_the_scan() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    // 300 manifests of 60 KiB each exceed the 16 MiB total.
    for index in 0..300 {
        let id = format!("session-{index:03}");
        let session = Session::new(root.path(), &id, "closed");
        write(
            &session.record(CoreRecord::Manifest),
            &json!({"schema": 1, "id": id, "provider": "codex", "provider_path": "codex",
                "provider_version": "1", "workspace": root.path(),
                "title": "x".repeat(60 * 1024), "yolo": false, "created_unix_ms": 1}),
        );
    }
    let scan = open_children(&mut environment, root.path(), "session-origin");
    assert!(scan.incomplete);
    assert_eq!(scan.incomplete_reasons.last().unwrap().reason, "byte_limit");
    assert!(scan.scanned.sessions < 300);
}

#[test]
fn a_synthetic_root_of_thousands_of_sessions_is_scanned_within_the_bounds() {
    let hooks = Hooks::new();
    let root = tempfile::tempdir().unwrap();
    let mut expected = Vec::new();
    // 3,276 sessions as in the observed root: most closed with many follow-ups.
    for index in 0..3276 {
        let id = format!("session-{index:04}");
        let (state, source) = match index % 100 {
            0 => ("ready", "session-origin"),
            1 => ("ready", "external"),
            2 => ("working", "session-else"),
            _ => ("closed", "session-origin"),
        };
        let session = Session::started_from(root.path(), &id, state, source);
        if state == "closed" {
            session.tombstone();
            for follow_up in 0..3 {
                session.receipt(&format!("{}-2", 200 + follow_up), 1, Some("session-else"));
            }
        } else if source == "session-origin" {
            expected.push(id);
        }
    }
    Session::new(root.path(), "session-origin", "closed").tombstone();
    let started = Instant::now();
    // A generous deadline keeps a loaded runner from turning this into a timing test; the
    // elapsed time is printed for the record.
    let scan = scan_started_from(
        root.path(),
        "session-origin",
        started + Duration::from_secs(60),
        &mut SystemClock,
    );
    let elapsed = started.elapsed();
    eprintln!("synthetic started-from scan of 3,277 entries: {elapsed:?}");
    assert!(!scan.incomplete, "{:?}", scan.incomplete_reasons);
    assert_eq!(scan.scanned.sessions, 3276);
    assert_eq!(scan.open, expected);
    // One receipt per session that is not closed; none for closed ones.
    assert_eq!(hooks.receipt_reads(), 3 * 33);
}

#[test]
fn an_owner_record_that_changes_or_disappears_before_the_recheck_is_busy() {
    let hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let child = Session::started_from(root.path(), "session-child", "ready", "session-origin");
    let origin = Session::new(root.path(), "session-origin", "ready");
    environment.attest(|_| {
        attestation(
            ProcessObservation::Gone(Gone::Exited),
            ControlAttestation::NotRecorded,
        )
    });
    // Unchanged, the attested owner decides.
    origin.owner(Some("session-origin"), 7);
    assert_eq!(
        observed(&mut environment, root.path(), &child).reason,
        "owner_exited"
    );
    // A new owner is recorded between the attestation and the recheck.
    let owner = origin.record(CoreRecord::Owner);
    let replaced = owner.clone();
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::OriginRead {
            write(
                &replaced,
                &json!({"pid": 8, "managed_session_id": "session-origin"}),
            );
        }
    });
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(
        (observation.ended, observation.reason.as_str()),
        (None, "origin_busy")
    );
    // The owner record is removed between the attestation and the recheck.
    origin.owner(Some("session-origin"), 7);
    let removed = owner.clone();
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::OriginRead {
            fs::remove_file(&removed).unwrap();
        }
    });
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(
        (observation.ended, observation.reason.as_str()),
        (None, "origin_busy")
    );
    // A verdict from "no owner record" is busy once an owner record appears.
    let appeared = owner.clone();
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::OriginRead {
            write(
                &appeared,
                &json!({"pid": 9, "managed_session_id": "session-origin"}),
            );
        }
    });
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(
        (observation.ended, observation.reason.as_str()),
        (None, "origin_busy")
    );
}

/// A manifest of exactly `RECORD_LIMIT` bytes that is not UTF-8: the reader consumes it
/// whole before it refuses it.
fn malformed_manifest(session: &Session) {
    fs::write(
        session.record(CoreRecord::Manifest),
        vec![0xff; RECORD_LIMIT as usize],
    )
    .unwrap();
}

#[test]
fn failed_reads_are_charged_against_the_cumulative_allowance() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    // The observation allowance holds eight full records; each failed read reserves one.
    let child = Session::started_from(root.path(), "session-child", "ready", "session-origin");
    let origin = Session::new(root.path(), "session-origin", "ready");
    malformed_manifest(&origin);
    let reference = read(&mut environment, &child);
    let mut budget = ReadBudget::new(
        environment.now() + Duration::from_secs(60),
        OBSERVATION_BYTES,
    );
    let allowance = OBSERVATION_BYTES / RECORD_LIMIT;
    for _ in 0..allowance {
        let observation = observe(
            root.path(),
            reference.clone(),
            &mut budget,
            &mut environment.origin(),
        );
        assert_eq!(observation.reason, "origin_unreadable");
    }
    let observation = observe(
        root.path(),
        reference,
        &mut budget,
        &mut environment.origin(),
    );
    assert_eq!(
        (observation.ended, observation.reason.as_str()),
        (None, "origin_budget_exhausted")
    );

    // The scan: more malformed 64 KiB manifests than the 16 MiB allowance holds.
    let scan_root = tempfile::tempdir().unwrap();
    let records = SCAN_BYTES / RECORD_LIMIT;
    for index in 0..records + 4 {
        let session = Session::new(scan_root.path(), &format!("session-{index:04}"), "ready");
        malformed_manifest(&session);
    }
    let scan = open_children(&mut environment, scan_root.path(), "session-origin");
    // Each of the first `records` sessions is a reason, then the byte limit stops the
    // scan at the next one; the reasons past the cap are only counted.
    assert!(scan.incomplete);
    assert_eq!(scan.scanned.sessions, records + 1);
    assert_eq!(
        scan.additional_incomplete_reasons,
        records + 1 - SCAN_REASONS as u64
    );
    // With fewer sessions than reasons the last one is visible.
    let small_root = tempfile::tempdir().unwrap();
    for index in 0..4 {
        let session = Session::new(small_root.path(), &format!("session-{index}"), "ready");
        malformed_manifest(&session);
    }
    let deadline = environment.now() + Duration::from_secs(60);
    let scan = scan_started_from(
        small_root.path(),
        "session-origin",
        deadline,
        &mut environment.clock,
    );
    assert!(
        !scan
            .incomplete_reasons
            .iter()
            .any(|r| r.reason == "byte_limit")
    );
}

#[test]
fn a_short_invalid_utf8_record_is_charged_the_whole_attempt() {
    let _hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    let origin = Session::new(root.path(), "session-origin", "ready");
    let status = origin.reader().record(CoreRecord::Status);
    let mut budget = ReadBudget::new(
        environment.now() + Duration::from_secs(60),
        OBSERVATION_BYTES,
    );
    // Two bytes that are not UTF-8: the reader refuses them, and the whole attempt is
    // reserved, as for a record that fills the per-record limit.
    fs::write(status.path(), b"\xff\xfe").unwrap();
    let fault = budget
        .decoded(&status, &mut environment.clock, record::status)
        .err()
        .unwrap();
    assert!(
        matches!(&fault, ReadFault::Unreadable(error) if error.contains("invalid UTF-8")),
        "{fault:?}"
    );
    assert_eq!(budget.remaining, OBSERVATION_BYTES - RECORD_LIMIT);
    // Two bytes the reader accepts and the decoder refuses cost only what was read.
    fs::write(status.path(), b"{}").unwrap();
    let fault = budget
        .decoded(&status, &mut environment.clock, record::status)
        .err()
        .unwrap();
    assert!(
        matches!(&fault, ReadFault::Unreadable(error) if error.starts_with("invalid JSON in ")),
        "{fault:?}"
    );
    assert_eq!(budget.remaining, OBSERVATION_BYTES - RECORD_LIMIT - 2);
    // Through the observation the fault is the record's, not the budget's.
    fs::write(status.path(), b"\xff\xfe").unwrap();
    let child = Session::started_from(root.path(), "session-child", "ready", "session-origin");
    let observation = observed(&mut environment, root.path(), &child);
    assert_eq!(observation.reason, "origin_unreadable");
    assert!(
        observation
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("invalid UTF-8")),
        "{observation:?}"
    );
}

#[test]
fn reaching_a_scan_cap_exactly_stops_and_marks_the_scan_incomplete() {
    let hooks = Hooks::new();
    let mut environment = Environment::new();
    let root = tempfile::tempdir().unwrap();
    // Exactly `SCAN_ENTRIES` entries: each is examined, then the limit is recorded.
    let entries = (0..SCAN_ENTRIES)
        .map(|index| {
            Ok(crate::native::session::enumerate::TestEntry {
                id: format!("session-{index:05}"),
                directory: root.path().join(format!("session-{index:05}")),
                directory_kind: Ok(true),
            })
        })
        .collect();
    let scan = crate::native::session::enumerate::with_entries(entries, || {
        open_children(&mut environment, root.path(), "session-origin")
    });
    assert!(scan.incomplete);
    assert_eq!(scan.scanned.sessions, SCAN_ENTRIES);
    assert_eq!(
        scan.additional_incomplete_reasons,
        SCAN_ENTRIES + 1 - SCAN_REASONS as u64
    );

    // Exactly `SCAN_OPEN` matches: all listed, the limit recorded, nothing read after.
    for index in 0..SCAN_OPEN {
        Session::started_from(
            root.path(),
            &format!("session-{index:04}"),
            "ready",
            "session-origin",
        );
    }
    let examined = Rc::new(Cell::new(0u64));
    let counted = examined.clone();
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::Entry {
            counted.set(counted.get() + 1);
        }
    });
    let scan = open_children(&mut environment, root.path(), "session-origin");
    assert!(scan.incomplete);
    assert_eq!(scan.open.len(), SCAN_OPEN);
    assert_eq!(scan.scanned.listed, SCAN_OPEN as u64);
    assert_eq!(scan.incomplete_reasons.len(), 1);
    assert!(scan.incomplete_reasons[0].reason.starts_with("open_limit"));
    assert_eq!(examined.get(), SCAN_OPEN as u64);

    // One more match: the scan still stops at the cap and reads no further entry.
    Session::started_from(root.path(), "session-0256", "ready", "session-origin");
    examined.set(0);
    let scan = open_children(&mut environment, root.path(), "session-origin");
    assert_eq!(scan.open.len(), SCAN_OPEN);
    assert_eq!(scan.scanned.sessions, SCAN_OPEN as u64);
    assert_eq!(examined.get(), SCAN_OPEN as u64);
}
