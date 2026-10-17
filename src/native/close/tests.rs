//! `close-session --with-started-from`: the loop over the scan's candidates with a fake
//! closer and a fake clock, and the real per-target close for the reason guarantee.
use super::*;
use crate::native::clock::FakeClock;
use crate::native::session::origin::scan_started_from;
use crate::native::session::origin::tests::{Checkpoint, Hooks, Session, write};
use crate::native::session::primitives::unix_ms;
use crate::native::session::{CoreRecord, SessionStatus, turn};
use crate::native::tests::reaped_child_pid;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

const ORIGIN: &str = "session-origin";

type Calls = Rc<RefCell<Vec<(String, String)>>>;

struct Fixture {
    root: tempfile::TempDir,
    clock: FakeClock,
    deadline: Instant,
    calls: Calls,
}

impl Fixture {
    fn new() -> Self {
        let clock = FakeClock::new(Instant::now());
        let deadline = clock.shared().get() + Duration::from_secs(60);
        let root = tempfile::tempdir().unwrap();
        Session::new(root.path(), ORIGIN, "closed").tombstone();
        Self {
            root,
            clock,
            deadline,
            calls: Rc::default(),
        }
    }
    fn path(&self) -> &Path {
        self.root.path()
    }
    fn child(&self, id: &str) -> Session {
        Session::started_from(self.path(), id, "ready", ORIGIN)
    }
    fn scan(&mut self) -> StartedFrom {
        let deadline = self.deadline;
        let root = self.root.path().to_owned();
        scan_started_from(&root, ORIGIN, deadline, &mut self.clock)
    }
    /// The loop with a closer that marks the target closed the way a completed close
    /// leaves its records: the tombstone and the closed status.
    fn close(&mut self, scan: &StartedFrom) -> StartedFromClose {
        let calls = Rc::clone(&self.calls);
        let mut closer = move |directory: &Path, id: &str, reason: String| {
            calls.borrow_mut().push((id.to_owned(), reason));
            mark_closed(directory);
            Ok(())
        };
        self.close_with(scan, &mut closer)
    }
    fn close_with(
        &mut self,
        scan: &StartedFrom,
        closer: &mut dyn FnMut(&Path, &str, String) -> Result<()>,
    ) -> StartedFromClose {
        let deadline = self.deadline;
        let root = self.root.path().to_owned();
        close_started_from(&root, ORIGIN, scan, deadline, &mut self.clock, closer)
    }
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.borrow().clone()
    }
}

fn mark_closed(directory: &Path) {
    let session = Session {
        directory: directory.to_owned(),
    };
    session.tombstone();
    session.status("closed", 9);
}

fn reason() -> String {
    started_from_reason(ORIGIN)
}

fn status_of(session: &Session) -> SessionStatus {
    serde_json::from_slice(&fs::read(session.record(CoreRecord::Status)).unwrap()).unwrap()
}

fn skipped(outcome: &StartedFromClose) -> Vec<(String, String)> {
    outcome
        .skipped
        .iter()
        .map(|t| (t.session.clone(), t.reason.clone()))
        .collect()
}

fn failed(outcome: &StartedFromClose) -> Vec<(String, String)> {
    outcome
        .failed
        .iter()
        .map(|t| (t.session.clone(), t.error.clone()))
        .collect()
}

fn closed(outcome: &StartedFromClose) -> Vec<String> {
    outcome.closed.iter().map(|t| t.session.clone()).collect()
}

fn tree(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn walk(path: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        if fs::symlink_metadata(path).unwrap().is_dir() {
            out.push((path.to_owned(), None));
            let mut entries: Vec<_> = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for entry in entries {
                walk(&entry, out);
            }
        } else {
            out.push((path.to_owned(), fs::read(path).ok()));
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn parsed(args: &[&str]) -> Result<CloseRequest> {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    match parse(&args)? {
        NativeCommand::Close(request) => Ok(request),
        _ => unreachable!(),
    }
}

#[test]
fn the_flag_is_parsed_once_and_absent_by_default() {
    let request = parsed(&["session-a", "--explicit"]).unwrap();
    assert!(!request.with_started_from);
    assert!(!request.json);
    let request = parsed(&[
        "session-a",
        "--explicit",
        "--with-started-from",
        "--json",
        "--timeout-secs",
        "5",
    ])
    .unwrap();
    assert!(request.with_started_from);
    assert!(request.json);
    assert_eq!(request.timeout, Duration::from_secs(5));
    let error = parsed(&[
        "session-a",
        "--explicit",
        "--with-started-from",
        "--with-started-from",
    ])
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "--with-started-from may only be specified once"
    );
    // The flag does not stand in for --explicit.
    let error = parsed(&["session-a", "--with-started-from"]).unwrap_err();
    assert!(error.to_string().contains("--explicit"), "{error}");
}

#[test]
fn a_candidate_closed_since_the_scan_is_skipped_without_a_closer_call() {
    let _hooks = Hooks::new();
    let mut fixture = Fixture::new();
    let done = fixture.child("session-done");
    let partial = fixture.child("session-partial");
    let scan = fixture.scan();
    assert_eq!(scan.open, vec!["session-done", "session-partial"]);
    // A completed close writes both records; a tombstone alone is a close in progress.
    done.tombstone();
    done.status("closed", 4);
    partial.tombstone();
    let outcome = fixture.close(&scan);
    assert_eq!(
        skipped(&outcome),
        vec![
            ("session-done".to_owned(), "closed_since_scan".to_owned()),
            ("session-partial".to_owned(), "close_in_progress".to_owned()),
        ]
    );
    assert!(outcome.closed.is_empty() && outcome.failed.is_empty());
    assert!(fixture.calls().is_empty());
}

#[test]
fn a_candidate_that_closes_during_the_recheck_is_skipped() {
    let hooks = Hooks::new();
    let mut fixture = Fixture::new();
    fixture.child("session-a");
    let scan = fixture.scan();
    // The scan has passed; the tombstone and the closed status land after the match of
    // the verification, before its recheck.
    hooks.checkpoint(|point, directory| {
        if point == Checkpoint::Matched {
            mark_closed(directory);
        }
    });
    let outcome = fixture.close(&scan);
    assert_eq!(
        skipped(&outcome),
        vec![("session-a".to_owned(), "closed_since_scan".to_owned())]
    );
    assert!(fixture.calls().is_empty());
}

#[test]
fn a_changed_or_invalid_source_is_skipped() {
    let _hooks = Hooks::new();
    let mut fixture = Fixture::new();
    let other = fixture.child("session-other");
    let external = fixture.child("session-external");
    let invalid = fixture.child("session-invalid");
    let own = fixture.child("session-own");
    let scan = fixture.scan();
    assert_eq!(scan.open.len(), 4);
    other.receipt("100-1", 100, Some("session-else"));
    external.receipt("100-1", 100, Some("external"));
    invalid.receipt("100-1", 100, Some("../x"));
    own.receipt("100-1", 100, Some("session-own"));
    let outcome = fixture.close(&scan);
    assert_eq!(
        skipped(&outcome),
        vec![
            ("session-external".to_owned(), "source_changed".to_owned()),
            (
                "session-invalid".to_owned(),
                "invalid_source: the recorded source is not a session id".to_owned()
            ),
            ("session-other".to_owned(), "source_changed".to_owned()),
            (
                "session-own".to_owned(),
                "invalid_source: the initial request names this session as its own source"
                    .to_owned()
            ),
        ]
    );
    assert!(fixture.calls().is_empty());
}

#[test]
fn a_candidate_that_disappeared_or_was_replaced_is_skipped() {
    let _hooks = Hooks::new();
    let mut fixture = Fixture::new();
    let gone = fixture.child("session-gone");
    let file = fixture.child("session-file");
    #[cfg(unix)]
    let linked = fixture.child("session-linked");
    let scan = fixture.scan();
    fs::remove_dir_all(&gone.directory).unwrap();
    fs::remove_dir_all(&file.directory).unwrap();
    fs::write(&file.directory, b"not a session").unwrap();
    #[cfg(unix)]
    {
        let elsewhere = fixture.path().join("elsewhere");
        fs::rename(&linked.directory, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &linked.directory).unwrap();
    }
    let outcome = fixture.close(&scan);
    let replaced = "session_gone: refusing a linked or non-directory session path".to_owned();
    let expected = [
        Some(("session-file".to_owned(), replaced.clone())),
        Some(("session-gone".to_owned(), "session_gone".to_owned())),
        cfg!(unix).then(|| ("session-linked".to_owned(), replaced)),
    ];
    assert_eq!(
        skipped(&outcome),
        expected.into_iter().flatten().collect::<Vec<_>>()
    );
    assert!(fixture.calls().is_empty());
}

type Mutation = Box<dyn Fn(&Session)>;

/// Every record the evidence covers, changed in a way the scan's own recheck would not
/// see, and the skip reason it earns.
fn mutations() -> Vec<(&'static str, Mutation, &'static str)> {
    vec![
        (
            "status generation",
            Box::new(|s| s.status("ready", 4)),
            "session_changed: status generation",
        ),
        (
            "status bytes",
            Box::new(|s| {
                write(
                    &s.record(CoreRecord::Status),
                    &json!({"state": "ready", "generation": 3, "updated_unix_ms": 2,
                        "exit_code": null, "error": null}),
                )
            }),
            "session_changed: status",
        ),
        (
            "launch claim",
            Box::new(|s| {
                s.launch("200-1");
                s.receipt("200-1", 100, Some(ORIGIN));
            }),
            "session_changed: launch claim",
        ),
        (
            "initial receipt",
            Box::new(|s| s.receipt("100-1", 101, Some(ORIGIN))),
            "session_changed: initial receipt",
        ),
        (
            "claim appeared",
            Box::new(|s| fs::write(s.record(CoreRecord::TurnClaim), "300-1\n").unwrap()),
            "session_changed: claim",
        ),
    ]
}

#[test]
fn every_record_changed_between_the_scan_and_the_verification_is_a_skip() {
    let _hooks = Hooks::new();
    for (what, mutate, expected) in mutations() {
        let mut fixture = Fixture::new();
        let child = fixture.child("session-a");
        let scan = fixture.scan();
        mutate(&child);
        let outcome = fixture.close(&scan);
        assert_eq!(
            skipped(&outcome),
            vec![("session-a".to_owned(), expected.to_owned())],
            "{what}"
        );
        assert!(fixture.calls().is_empty(), "{what}");
    }
    // A Claim that changed or disappeared since the scan is a change too.
    for (what, after) in [("changed", Some("301-1\n")), ("removed", None)] {
        let mut fixture = Fixture::new();
        let child = fixture.child("session-a");
        fs::write(child.record(CoreRecord::TurnClaim), "300-1\n").unwrap();
        let scan = fixture.scan();
        match after {
            Some(token) => fs::write(child.record(CoreRecord::TurnClaim), token).unwrap(),
            None => fs::remove_file(child.record(CoreRecord::TurnClaim)).unwrap(),
        }
        let outcome = fixture.close(&scan);
        assert_eq!(
            skipped(&outcome),
            vec![("session-a".to_owned(), "session_changed: claim".to_owned())],
            "claim {what}"
        );
    }
}

#[test]
fn every_record_changed_inside_the_verification_is_a_skip() {
    for (what, mutate, expected) in mutations() {
        let hooks = Hooks::new();
        let mut fixture = Fixture::new();
        let child = fixture.child("session-a");
        let scan = fixture.scan();
        // The verification has admitted the directory; the change lands before its reads.
        let pending = Cell::new(Some(mutate));
        hooks.checkpoint(move |point, directory| {
            if point == Checkpoint::Verify
                && let Some(mutate) = pending.take()
            {
                mutate(&Session {
                    directory: directory.to_owned(),
                });
            }
        });
        let outcome = fixture.close(&scan);
        assert_eq!(
            skipped(&outcome),
            vec![("session-a".to_owned(), expected.to_owned())],
            "{what}"
        );
        assert!(fixture.calls().is_empty(), "{what}");
        drop(child);
    }
    // A status that changes after the verification's match is caught by its recheck.
    let hooks = Hooks::new();
    let mut fixture = Fixture::new();
    fixture.child("session-a");
    let scan = fixture.scan();
    hooks.checkpoint(|point, directory| {
        if point == Checkpoint::Matched {
            Session {
                directory: directory.to_owned(),
            }
            .status("claimed", 4);
        }
    });
    let outcome = fixture.close(&scan);
    assert_eq!(
        skipped(&outcome),
        vec![("session-a".to_owned(), "session_changed".to_owned())]
    );
    assert!(fixture.calls().is_empty());
}

#[test]
fn an_unchanged_pending_claim_is_allowed_and_never_replaces_the_launch_claim() {
    let _hooks = Hooks::new();
    let mut fixture = Fixture::new();
    let child = fixture.child("session-a");
    // The current turn came from elsewhere; the launch Claim still names the origin.
    fs::write(child.record(CoreRecord::TurnClaim), "300-1\n").unwrap();
    child.receipt("300-1", 300, Some("session-else"));
    child.status("working", 5);
    let scan = fixture.scan();
    assert_eq!(scan.open, vec!["session-a"]);
    let outcome = fixture.close(&scan);
    assert_eq!(closed(&outcome), vec!["session-a"]);
    assert_eq!(fixture.calls(), vec![("session-a".to_owned(), reason())]);
}

#[test]
fn expiry_before_during_and_after_the_verification_is_a_failure() {
    // Before: the shared deadline passed before the first target; nothing is verified.
    let hooks = Hooks::new();
    let mut fixture = Fixture::new();
    fixture.child("session-a");
    fixture.child("session-b");
    let scan = fixture.scan();
    let verifications = Rc::new(Cell::new(0));
    let seen = Rc::clone(&verifications);
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::Verify {
            seen.set(seen.get() + 1);
        }
    });
    fixture.clock.sleep(Duration::from_secs(120));
    let outcome = fixture.close(&scan);
    assert_eq!(
        failed(&outcome),
        vec![
            ("session-a".to_owned(), EXHAUSTED.to_owned()),
            ("session-b".to_owned(), EXHAUSTED.to_owned()),
        ]
    );
    assert_eq!(verifications.get(), 0);
    assert!(fixture.calls().is_empty());

    // During: the clock passes the deadline between two reads of the verification.
    let hooks = Hooks::new();
    let mut fixture = Fixture::new();
    fixture.child("session-a");
    let scan = fixture.scan();
    let clock = fixture.clock.shared();
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::Matched {
            clock.set(clock.get() + Duration::from_secs(120));
        }
    });
    let outcome = fixture.close(&scan);
    assert_eq!(
        failed(&outcome),
        vec![("session-a".to_owned(), EXHAUSTED.to_owned())]
    );
    assert!(fixture.calls().is_empty());

    // After: the verification passed, the close itself found the budget gone and
    // refused; the next target never reaches its verification.
    let hooks = Hooks::new();
    let mut fixture = Fixture::new();
    fixture.child("session-a");
    fixture.child("session-b");
    let scan = fixture.scan();
    let verifications = Rc::new(Cell::new(0));
    let seen = Rc::clone(&verifications);
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::Verify {
            seen.set(seen.get() + 1);
        }
    });
    let clock = fixture.clock.shared();
    let calls = Rc::clone(&fixture.calls);
    let mut closer = move |_: &Path, id: &str, reason: String| {
        calls.borrow_mut().push((id.to_owned(), reason));
        clock.set(clock.get() + Duration::from_secs(120));
        Err(anyhow::anyhow!(
            "close deadline expired; closing was not confirmed"
        ))
    };
    let outcome = fixture.close_with(&scan, &mut closer);
    assert_eq!(
        failed(&outcome),
        vec![
            (
                "session-a".to_owned(),
                "close deadline expired; closing was not confirmed".to_owned()
            ),
            ("session-b".to_owned(), EXHAUSTED.to_owned()),
        ]
    );
    assert_eq!(verifications.get(), 1);
    assert_eq!(fixture.calls().len(), 1);
}

#[test]
fn a_failing_closer_leaves_the_others_attempted_in_order() {
    let _hooks = Hooks::new();
    let mut fixture = Fixture::new();
    fixture.child("session-c");
    fixture.child("session-a");
    fixture.child("session-b");
    let scan = fixture.scan();
    let calls = Rc::clone(&fixture.calls);
    let mut closer = move |directory: &Path, id: &str, reason: String| {
        calls.borrow_mut().push((id.to_owned(), reason));
        if id == "session-b" {
            return Err(anyhow::anyhow!("deeper").context("boom"));
        }
        mark_closed(directory);
        Ok(())
    };
    let outcome = fixture.close_with(&scan, &mut closer);
    assert_eq!(closed(&outcome), vec!["session-a", "session-c"]);
    assert_eq!(
        failed(&outcome),
        vec![("session-b".to_owned(), "boom: deeper".to_owned())]
    );
    assert!(outcome.skipped.is_empty());
    assert_eq!(
        fixture.calls(),
        vec![
            ("session-a".to_owned(), reason()),
            ("session-b".to_owned(), reason()),
            ("session-c".to_owned(), reason()),
        ]
    );
    let empty = json!({"open": [], "incomplete": false, "incomplete_reasons": [],
        "additional_incomplete_reasons": 0, "scanned": {"sessions": 3, "listed": 0}});
    assert_eq!(
        outcome.json(ORIGIN, &scan),
        json!({"ok": false, "session": ORIGIN, "closed": true, "started_from": {
            "open": ["session-a", "session-b", "session-c"], "incomplete": false,
            "incomplete_reasons": [], "additional_incomplete_reasons": 0,
            "scanned": {"sessions": 3, "listed": 3},
            "closed": [
                {"session": "session-a", "started_from": empty},
                {"session": "session-c", "started_from": empty}],
            "skipped": [],
            "failed": [{"session": "session-b", "error": "boom: deeper"}]}})
    );
    assert_eq!(
        outcome.lines(ORIGIN, &scan),
        vec![
            "started-from candidates: 3 (scan complete)",
            "closed session-a (started from session-origin); its own started-from: 0 open",
            "failed session-b: boom: deeper",
            "closed session-c (started from session-origin); its own started-from: 0 open",
            "started-from close: 2 closed, 0 skipped, 1 failed",
        ]
    );
}

#[test]
fn a_closed_target_reports_its_own_started_from_without_closing_it() {
    let _hooks = Hooks::new();
    let mut fixture = Fixture::new();
    fixture.child("session-child");
    let grandchild = Session::started_from(
        fixture.path(),
        "session-grandchild",
        "working",
        "session-child",
    );
    let before = tree(&grandchild.directory);
    let scan = fixture.scan();
    assert_eq!(scan.open, vec!["session-child"]);
    let outcome = fixture.close(&scan);
    assert_eq!(closed(&outcome), vec!["session-child"]);
    let own = &outcome.closed[0].started_from;
    assert_eq!(own.open, vec!["session-grandchild"]);
    assert!(!own.incomplete, "{:?}", own.incomplete_reasons);
    assert_eq!(fixture.calls().len(), 1);
    assert_eq!(tree(&grandchild.directory), before);
    assert_eq!(
        outcome.lines(ORIGIN, &scan)[1],
        "closed session-child (started from session-origin); its own started-from: 1 open"
    );
    let value = outcome.json(ORIGIN, &scan);
    assert_eq!(value["ok"], true);
    assert_eq!(
        value["started_from"]["closed"][0]["started_from"]["open"],
        json!(["session-grandchild"])
    );
    // The target's report runs within the shared deadline: none left, none read.
    let mut fixture = Fixture::new();
    fixture.child("session-child");
    Session::started_from(
        fixture.path(),
        "session-grandchild",
        "working",
        "session-child",
    );
    let scan = fixture.scan();
    let clock = fixture.clock.shared();
    let deadline = fixture.deadline;
    let calls = Rc::clone(&fixture.calls);
    let mut closer = move |directory: &Path, id: &str, reason: String| {
        calls.borrow_mut().push((id.to_owned(), reason));
        mark_closed(directory);
        clock.set(deadline);
        Ok(())
    };
    let outcome = fixture.close_with(&scan, &mut closer);
    let own = &outcome.closed[0].started_from;
    assert!(own.open.is_empty());
    assert!(own.incomplete);
    assert_eq!(own.incomplete_reasons[0].reason, EXHAUSTED);
    assert_eq!(
        outcome.lines(ORIGIN, &scan)[1],
        "closed session-child (started from session-origin); its own started-from: 0 open, incomplete"
    );
}

#[test]
fn an_exhausted_scan_still_closes_what_it_listed() {
    // Zero candidates: the report is incomplete and nothing fails.
    let hooks = Hooks::new();
    let mut fixture = Fixture::new();
    fixture.child("session-a");
    let now = fixture.clock.shared().get();
    let root = fixture.path().to_owned();
    let scan = scan_started_from(&root, ORIGIN, now, &mut fixture.clock);
    assert!(scan.incomplete && scan.open.is_empty());
    let outcome = fixture.close(&scan);
    assert!(outcome.closed.is_empty() && outcome.skipped.is_empty() && outcome.failed.is_empty());
    assert_eq!(outcome.json(ORIGIN, &scan)["ok"], true);
    assert_eq!(
        outcome.lines(ORIGIN, &scan),
        vec![
            "started-from candidates: 0 (scan incomplete)",
            "started-from close: 0 closed, 0 skipped, 0 failed",
        ]
    );
    // Some candidates: the scan's own cap passes at the third entry while the command's
    // deadline has time left; the two listed are verified and closed.
    for index in 0..5 {
        fixture.child(&format!("session-{index}"));
    }
    let entries = Rc::new(Cell::new(0));
    let seen = Rc::clone(&entries);
    let clock = fixture.clock.shared();
    let scan_deadline = clock.get() + Duration::from_secs(2);
    hooks.checkpoint(move |point, _| {
        if point == Checkpoint::Entry {
            seen.set(seen.get() + 1);
            if seen.get() == 3 {
                clock.set(scan_deadline);
            }
        }
    });
    let scan = scan_started_from(&root, ORIGIN, scan_deadline, &mut fixture.clock);
    assert!(scan.incomplete);
    assert_eq!(scan.incomplete_reasons[0].reason, EXHAUSTED);
    assert_eq!(scan.open.len(), 2);
    let outcome = fixture.close(&scan);
    assert_eq!(closed(&outcome), scan.open);
    assert!(outcome.failed.is_empty());
    assert_eq!(fixture.calls().len(), 2);
    // With the command's deadline gone too, every listed target fails.
    fixture.clock.sleep(Duration::from_secs(120));
    let outcome = fixture.close(&scan);
    assert_eq!(
        failed(&outcome),
        scan.open
            .iter()
            .map(|id| (id.clone(), EXHAUSTED.to_owned()))
            .collect::<Vec<_>>()
    );
    assert_eq!(fixture.calls().len(), 2);
}

/// The production closer: the ordinary explicit close of the target with the reason.
fn real_closer(deadline: Instant) -> impl FnMut(&Path, &str, String) -> Result<()> {
    move |directory, id, reason| close_session_surface(directory, id, Some(reason), deadline)
}

#[test]
fn an_ownerless_handle_less_target_keeps_the_supplied_reason() {
    let _hooks = Hooks::new();
    let mut fixture = Fixture::new();
    let child = fixture.child("session-a");
    let scan = fixture.scan();
    let outcome = fixture.close_with(&scan, &mut real_closer(Instant::now() + CLOSE_TIMEOUT));
    assert_eq!(closed(&outcome), vec!["session-a"]);
    let status = status_of(&child);
    assert_eq!(status.state.as_str(), "closed");
    assert_eq!(status.error.as_deref(), Some(reason().as_str()));
    assert!(child.record(CoreRecord::Closed).exists());
}

#[test]
fn a_reaped_owner_target_is_closed_and_keeps_its_repair_reason() {
    let _hooks = Hooks::new();
    let mut fixture = Fixture::new();
    let child = Session::started_from(fixture.path(), "session-a", "working", ORIGIN);
    turn::claim(&Store::open_unchecked(&child.directory), &[])
        .unwrap()
        .retain();
    child.owner(Some("session-a"), reaped_child_pid());
    let scan = fixture.scan();
    assert_eq!(scan.open, vec!["session-a"]);
    let outcome = fixture.close_with(&scan, &mut real_closer(Instant::now() + CLOSE_TIMEOUT));
    assert_eq!(closed(&outcome), vec!["session-a"], "{:?}", outcome.failed);
    let status = status_of(&child);
    assert_eq!(status.state.as_str(), "closed");
    // The repair wrote the tombstone first; its diagnostic is the committed one.
    let error = status.error.as_deref().unwrap_or_default();
    assert!(error.contains("no longer running"), "{error}");
    assert!(!error.contains("--with-started-from"), "{error}");
    assert!(!child.record(CoreRecord::TurnClaim).exists());
}

#[test]
fn a_working_target_with_a_pending_claim_is_closed() {
    let _hooks = Hooks::new();
    let mut fixture = Fixture::new();
    let child = Session::started_from(fixture.path(), "session-a", "working", ORIGIN);
    turn::claim(&Store::open_unchecked(&child.directory), &[])
        .unwrap()
        .retain();
    let scan = fixture.scan();
    assert_eq!(scan.open, vec!["session-a"]);
    let outcome = fixture.close_with(&scan, &mut real_closer(Instant::now() + CLOSE_TIMEOUT));
    assert_eq!(closed(&outcome), vec!["session-a"], "{:?}", outcome.failed);
    assert_eq!(status_of(&child).state.as_str(), "closed");
    assert!(!child.record(CoreRecord::TurnClaim).exists());
}

#[test]
fn a_launching_target_with_an_unexpired_pending_launch_fails_with_the_ordinary_refusal() {
    let _hooks = Hooks::new();
    let mut fixture = Fixture::new();
    let child = Session::new(fixture.path(), "session-a", "launching");
    let launch_deadline = unix_ms() + 600_000;
    write(
        &child.record(CoreRecord::Launch),
        &json!({"schema": 1, "claim_token": "100-1", "deadline_unix_ms": launch_deadline,
            "phase": "pending"}),
    );
    child.receipt("100-1", 100, Some(ORIGIN));
    let scan = fixture.scan();
    assert_eq!(scan.open, vec!["session-a"]);
    let outcome = fixture.close_with(&scan, &mut real_closer(Instant::now() + CLOSE_TIMEOUT));
    assert!(outcome.closed.is_empty() && outcome.skipped.is_empty());
    let failures = failed(&outcome);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, "session-a");
    assert!(
        failures[0]
            .1
            .contains("the launcher is still creating the surface for this session"),
        "{}",
        failures[0].1
    );
    assert_eq!(status_of(&child).state.as_str(), "launching");
    assert!(!child.record(CoreRecord::Closed).exists());
    let value = outcome.json(ORIGIN, &scan);
    assert_eq!(value["ok"], false);
    assert_eq!(value["started_from"]["failed"][0]["session"], "session-a");
    assert_eq!(value["closed"], Value::Bool(true));
}
