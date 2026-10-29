use super::launch_failure::{isolated, isolated_with_output};
use super::*;
use serde_json::Value;
use std::process::{Child, Stdio};
use std::sync::{Arc, Barrier, mpsc};

const SOURCE: &str = "session-recovery-source";
const CHILD: &str = "session-recovery-child";
const FAILURE: &str = "issue 248 launcher failure";

fn request() -> ReopenRequest {
    let NativeCommand::Reopen(request) = parse_args([
        "reopen",
        SOURCE,
        "--prompt",
        "fixture",
        "--terminal",
        "windows-console",
        "--yolo",
        "--json",
        "--timeout-secs",
        "30",
    ])
    .unwrap() else {
        panic!("expected reopen")
    };
    request
}

fn source(root: &Path) -> PathBuf {
    write_closed_reopen_source(root, SOURCE, "claude", Some(REOPEN_TEST_CONVERSATION), true)
}

fn refusal(token: &str) -> Value {
    json!({"schema":3,"phase":"launch","gate":"launch-failed","detail":FAILURE,
        "created_unix_ms":1,"claim_token":token})
}

fn interrupted(root: &Path) -> (PathBuf, PathBuf, String) {
    let source = source(root);
    let child = write_reopen_launch_session(root, CHILD, Path::new("claude"));
    Reservation::claim(&source, SOURCE, REOPEN_TEST_CONVERSATION)
        .unwrap()
        .finalize(CHILD)
        .unwrap();
    let store = Store::open_unchecked(&child);
    let claim = turn::claim(&store, &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    launch::begin(&store, &token, Instant::now() + Duration::from_secs(30)).unwrap();
    store
        .record(CoreRecord::ReopenRefusal)
        .write_json(&refusal(&token))
        .unwrap();
    (source, child, token)
}

fn fail_next() {
    terminal::launch_failure_test_support::inject(|_| anyhow::anyhow!(FAILURE));
}

fn created(root: &Path) -> PathBuf {
    fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.is_dir()
                && p.file_name().unwrap() != SOURCE
                && p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("session-")
        })
        .unwrap()
}

struct StandIn(Child);
impl Drop for StandIn {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        assert!(self.0.wait().unwrap().success());
    }
}

fn command() -> Command {
    let mut command = Command::new("cmd.exe");
    command
        .args(["/d", "/c", "pause"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[test]
fn interrupted_fence_recovers_only_its_claim_on_real_reopen() {
    if isolated(concat!(
        module_path!(),
        "::interrupted_fence_recovers_only_its_claim_on_real_reopen"
    )) {
        return;
    }
    let root = Reader::state_root().unwrap();
    let (source, child, token) = interrupted(&root);
    let before_source = snapshot_directory(&source);
    let before_child = snapshot_directory(&child);
    let mut checks = Vec::new();
    marker_check(&Reader::open_unchecked(&source), SOURCE, &mut checks);
    assert_eq!(snapshot_directory(&source), before_source);
    assert_eq!(snapshot_directory(&child), before_child);
    fs::write(child.join(TURN_CLAIM_FILE), b"replacement-claim").unwrap();
    let changed = snapshot_directory(&child);
    assert_eq!(
        reopen_refusal_gate(&run_reopen(request()).unwrap_err()),
        Some(ReopenGate::AlreadyReopened)
    );
    assert_eq!(snapshot_directory(&child), changed);
    fs::write(child.join(TURN_CLAIM_FILE), token.as_bytes()).unwrap();
    fail_next();
    let error = run_reopen(request()).unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some(ReopenGate::LaunchFailed),
        "{error:#}"
    );
    assert_eq!(
        Reader::open_unchecked(&child).status().unwrap().state,
        SessionState::Failed
    );
    assert!(!child.join(TURN_CLAIM_FILE).exists());
    assert!(!source.join(REOPEN_MARKER_FILE).exists());
}

#[test]
fn failed_fence_retry_preserves_refusal_gate_and_evidence() {
    if isolated_with_output(
        concat!(
            module_path!(),
            "::failed_fence_retry_preserves_refusal_gate_and_evidence"
        ),
        |output| assert!(output.contains("\"gate\": \"already-reopened\"")),
    ) {
        return;
    }
    let root = Reader::state_root().unwrap();
    let (source, child, token) = interrupted(&root);
    let refusal = read_reopen_launch_refusal(&child).unwrap();
    assert_eq!(refusal.schema, 3);
    assert_eq!(refusal.claim_token.as_deref(), Some(token.as_str()));
    assert_eq!(refusal.detail, FAILURE);
    let status = Store::open_unchecked(&child).record(CoreRecord::Status);
    fs::write(status.path(), b"{damaged").unwrap();
    let before_source = snapshot_directory(&source);
    let before_child = snapshot_directory(&child);
    let entries_before = fs::read_dir(&root).unwrap().count();
    let error = run_reopen(request()).unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some(ReopenGate::AlreadyReopened),
        "{error:#}"
    );
    let detail = format!("{error:#}");
    assert!(detail.contains("launch-failed"), "{detail}");
    assert!(
        detail.contains("its records cannot be verified"),
        "{detail}"
    );
    assert!(detail.contains(status.path().to_str().unwrap()), "{detail}");
    assert_eq!(snapshot_directory(&source), before_source);
    assert_eq!(snapshot_directory(&child), before_child);
    assert_eq!(fs::read_dir(&root).unwrap().count(), entries_before);
}

#[test]
fn failed_fence_retry_keeps_source_admission_precedence() {
    if isolated(concat!(
        module_path!(),
        "::failed_fence_retry_keeps_source_admission_precedence"
    )) {
        return;
    }
    let root = Reader::state_root().unwrap();
    let (source, child, _) = interrupted(&root);
    fs::write(
        Store::open_unchecked(&child)
            .record(CoreRecord::Status)
            .path(),
        b"{damaged",
    )
    .unwrap();
    for (state, gate) in [
        (SessionState::Ready, ReopenGate::SourceNotClosed),
        (SessionState::Closed, ReopenGate::SourceNotConverged),
    ] {
        let store = Store::open_unchecked(&source);
        let mut status = store.status().unwrap();
        status.state = state;
        store.write_status(&status).unwrap();
        write_private(&source.join(TURN_CLAIM_FILE), b"unconverged-source").unwrap();
        let before_source = snapshot_directory(&source);
        let before_child = snapshot_directory(&child);
        let error = run_reopen(request()).unwrap_err();
        assert_eq!(reopen_refusal_gate(&error), Some(gate), "{error:#}");
        assert_eq!(snapshot_directory(&source), before_source);
        assert_eq!(snapshot_directory(&child), before_child);
        fs::remove_file(source.join(TURN_CLAIM_FILE)).unwrap();
    }
}

#[test]
fn failed_refusal_write_keeps_typed_gate_and_marker() {
    if isolated_with_output(
        concat!(
            module_path!(),
            "::failed_refusal_write_keeps_typed_gate_and_marker"
        ),
        |output| {
            assert!(output.contains("\"gate\": \"launch-failed\""));
            assert!(output.contains(FAILURE));
        },
    ) {
        return;
    }
    let root = Reader::state_root().unwrap();
    let source = source(&root);
    terminal::launch_failure_test_support::inject(|directory| {
        fs::create_dir(directory.join(REOPEN_REFUSAL_FILE)).unwrap();
        anyhow::anyhow!(FAILURE)
    });
    let error = run_reopen(request()).unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some(ReopenGate::LaunchFailed),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains(FAILURE));
    assert!(format!("{error:#}").contains("refusal"));
    assert!(source.join(REOPEN_MARKER_FILE).exists());
    assert_eq!(
        reopen_refusal_gate(&run_reopen(request()).unwrap_err()),
        Some(ReopenGate::AlreadyReopened)
    );
}

#[test]
fn decoder_and_receipt_mismatch_never_release() {
    let root = tempfile::tempdir().unwrap();
    let (source, child, token) = interrupted(root.path());
    update_status(&child, SessionState::Failed, None, None).unwrap();
    for record in [
        json!({"schema":3,"phase":"launch","gate":"launch-failed","detail":"x","created_unix_ms":1}),
        refusal(""),
        refusal(" "),
        {
            let mut r = refusal(&token);
            r["gate"] = json!("provider-unsupported");
            r
        },
        {
            let mut r = refusal(&token);
            r["schema"] = json!(2);
            r
        },
        refusal("different-launch"),
    ] {
        Store::open_unchecked(&child)
            .record(CoreRecord::ReopenRefusal)
            .write_json(&record)
            .unwrap();
        assert_eq!(
            reopen_refusal_gate(&inspect_reopen_source(&source, SOURCE).unwrap_err()),
            Some(ReopenGate::AlreadyReopened),
            "{record}"
        );
    }
}

#[test]
fn annotation_and_doctor_preserve_generic_refusal_identity() {
    let root = tempfile::tempdir().unwrap();
    let (source, child, token) = interrupted(root.path());
    for (state, phase, reason) in [
        (
            SessionState::Launching,
            launch::Phase::Pending,
            "reopen_marker_retained",
        ),
        (
            SessionState::Failed,
            launch::Phase::Spawning,
            "reopen_marker_retained",
        ),
        (
            SessionState::Failed,
            launch::Phase::Pending,
            "reopen_marker_reconcilable",
        ),
    ] {
        update_status(&child, state, None, None).unwrap();
        let store = Store::open_unchecked(&child);
        let mut receipt = launch::read(&store).unwrap().unwrap();
        receipt.phase = phase;
        store
            .record(CoreRecord::Launch)
            .write_json(&receipt)
            .unwrap();
        record_reopen_refusal_cleanup_pending(&child, "still pending").unwrap();
        let wire: Value = store.record(CoreRecord::ReopenRefusal).json().unwrap();
        assert_eq!(wire["schema"], 3);
        assert_eq!(wire["claim_token"], token);
        let before_source = snapshot_directory(&source);
        let before_child = snapshot_directory(&child);
        let mut checks = Vec::new();
        marker_check(&Reader::open_unchecked(&source), SOURCE, &mut checks);
        assert_eq!(checks[0].reason_code, reason);
        assert_eq!(snapshot_directory(&source), before_source);
        assert_eq!(snapshot_directory(&child), before_child);
    }
}

#[test]
fn specific_wrapper_refusal_wins_in_both_orders() {
    if isolated(concat!(
        module_path!(),
        "::specific_wrapper_refusal_wins_in_both_orders"
    )) {
        return;
    }
    let root = Reader::state_root().unwrap();
    let source = source(&root);
    terminal::launch_failure_test_support::inject(|directory| {
        record_reopen_refusal(
            directory,
            ReopenGate::ProviderUnsupported,
            "specific wrapper refusal".into(),
        );
        anyhow::anyhow!(FAILURE)
    });
    let error = run_reopen(request()).unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some(ReopenGate::ProviderUnsupported)
    );
    assert!(format!("{error:#}").contains(FAILURE));
    let child = created(&root);
    assert_eq!(read_reopen_launch_refusal(&child).unwrap().schema, 2);
    assert!(!source.join(REOPEN_MARKER_FILE).exists());
    let marker = Reservation::claim(&source, SOURCE, REOPEN_TEST_CONVERSATION).unwrap();
    marker
        .finalize(child.file_name().unwrap().to_str().unwrap())
        .unwrap();
    Store::open_unchecked(&child)
        .record(CoreRecord::ReopenRefusal)
        .write_json(&refusal("token"))
        .unwrap();
    record_reopen_refusal(
        &child,
        ReopenGate::ProviderUnsupported,
        "late wrapper refusal".into(),
    );
    assert!(inspect_reopen_source(&source, SOURCE).is_ok());
}

#[test]
fn spawn_commits_before_failure_preserves_live_states_then_releases_on_exit() {
    if isolated(concat!(
        module_path!(),
        "::spawn_commits_before_failure_preserves_live_states_then_releases_on_exit"
    )) {
        return;
    }
    let root = Reader::state_root().unwrap();
    let source = source(&root);
    for state in [SessionState::Running, SessionState::AwaitingInitialInput] {
        let expected = state.clone();
        let child_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = child_slot.clone();
        terminal::launch_failure_test_support::inject(move |directory| {
            launch::admit_wrapper(&Store::open_unchecked(directory)).unwrap();
            let owned = directory.to_owned();
            let (locked_tx, locked_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let store = Store::open_unchecked(&owned);
                launch::spawn(&store, &mut command(), |child| {
                    locked_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    record_provider_process(
                        &owned,
                        owned.file_name().unwrap().to_str().unwrap(),
                        child,
                    )?;
                    update_status(&owned, state, None, None)
                })
                .unwrap()
            });
            locked_rx.recv().unwrap();
            let error = launch::wait(
                &Store::open_unchecked(directory),
                &reopen_test_terminal("fixture"),
                Instant::now(),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("startup is still holding the lifecycle lock"));
            resume_tx.send(()).unwrap();
            *slot.borrow_mut() = Some(StandIn(worker.join().unwrap()));
            error
        });
        let error = run_reopen(request()).unwrap_err();
        assert_eq!(reopen_refusal_gate(&error), Some(ReopenGate::LaunchFailed));
        let marker: ReopenMarker = Reader::open_unchecked(&source)
            .record(CoreRecord::ReopenMarker)
            .json()
            .unwrap();
        let child = root.join(marker.reopened_by.unwrap());
        let store = Store::open_unchecked(&child);
        assert_eq!(store.status().unwrap().state, expected);
        assert!(child.join(TURN_CLAIM_FILE).exists());
        assert!(inspect_reopen_source(&source, SOURCE).is_err());
        drop(child_slot.borrow_mut().take());
        assert!(inspect_reopen_source(&source, SOURCE).is_ok());
        fail_next();
        let error = run_reopen(request()).unwrap_err();
        assert_eq!(reopen_refusal_gate(&error), Some(ReopenGate::LaunchFailed));
        assert!(!source.join(REOPEN_MARKER_FILE).exists());
    }
}

#[test]
fn launch_failed_waits_for_in_progress_spawn_and_preserves_committed_live_state() {
    use crate::native::session::test_support::close_io_test::{self, Point};

    for state in [SessionState::Running, SessionState::AwaitingInitialInput] {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        let child = write_reopen_launch_session(root.path(), CHILD, Path::new("claude"));
        Reservation::claim(&source, SOURCE, REOPEN_TEST_CONVERSATION)
            .unwrap()
            .finalize(CHILD)
            .unwrap();
        let created = CreatedSession {
            id: CHILD.into(),
            directory: child.clone(),
            manifest: Reader::open_unchecked(&child).manifest().unwrap(),
        };
        let store = Store::open_unchecked(&child);
        let mut claim = turn::claim(&store, &[]).unwrap();
        let token = claim.token().to_owned();
        launch::begin(&store, &token, Instant::now() + Duration::from_secs(30)).unwrap();
        launch::admit_wrapper(&store).unwrap();
        let before_source = snapshot_directory(&source);
        let expected = state.clone();
        let owned = child.clone();
        let (locked_tx, locked_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            StandIn(
                launch::spawn(&Store::open_unchecked(&owned), &mut command(), |process| {
                    locked_tx.send(()).unwrap();
                    resume_rx
                        .recv_timeout(Duration::from_secs(10))
                        .context("failure handler did not reach the blocking lock attempt")?;
                    record_provider_process(&owned, CHILD, process)?;
                    update_status(&owned, state, None, None)
                })
                .unwrap(),
            )
        });
        locked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let held = child.clone();
        let held_token = token.clone();
        let releaser = thread::spawn(move || {
            attempt_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let store = Store::open_unchecked(&held);
            assert!(store.try_lock().unwrap().is_none());
            assert_eq!(store.status().unwrap().state, SessionState::Launching);
            assert_eq!(
                launch::read(&store).unwrap().unwrap().phase,
                launch::Phase::Spawning
            );
            assert_eq!(
                turn::current_claim_token(&store).unwrap().as_deref(),
                Some(held_token.as_str())
            );
            assert!(!held.join(REOPEN_REFUSAL_FILE).exists());
            resume_tx.send(()).unwrap();
        });
        let mut attempt = Some(attempt_tx);
        let error = close_io_test::during(
            move |point| {
                if point == Point::BlockingLock
                    && let Some(attempt) = attempt.take()
                {
                    attempt.send(()).unwrap();
                }
            },
            || fail_reopen_launch(&created, &mut claim, anyhow::anyhow!(FAILURE)),
        );
        releaser.join().unwrap();
        let process = worker.join().unwrap();
        assert_eq!(reopen_refusal_gate(&error), Some(ReopenGate::LaunchFailed));
        assert!(format!("{error:#}").contains(FAILURE));
        assert_eq!(store.status().unwrap().state, expected);
        assert_eq!(
            launch::read(&store).unwrap().unwrap().phase,
            launch::Phase::Spawned
        );
        let refusal = read_reopen_launch_refusal(&child).unwrap();
        assert_eq!(refusal.claim_token.as_deref(), Some(token.as_str()));
        assert_eq!(refusal.detail, FAILURE);
        drop(claim);
        assert_eq!(
            turn::current_claim_token(&store).unwrap().as_deref(),
            Some(token.as_str())
        );
        assert_eq!(snapshot_directory(&source), before_source);
        assert_eq!(
            observe_provider_process(&store.provider_process().unwrap()),
            ProviderProcessObservation::Alive
        );
        assert_eq!(
            reopen_refusal_gate(&inspect_reopen_source(&source, SOURCE).unwrap_err()),
            Some(ReopenGate::AlreadyReopened)
        );
        assert_eq!(snapshot_directory(&source), before_source);
        drop(process);
    }
}

#[test]
fn fence_prevents_admitted_and_legacy_spawns() {
    if isolated(concat!(
        module_path!(),
        "::fence_prevents_admitted_and_legacy_spawns"
    )) {
        return;
    }
    let root = Reader::state_root().unwrap();
    source(&root);
    let (tx, rx) = mpsc::channel();
    terminal::launch_failure_test_support::inject(move |directory| {
        launch::admit_wrapper(&Store::open_unchecked(directory)).unwrap();
        tx.send(directory.to_owned()).unwrap();
        anyhow::anyhow!(FAILURE)
    });
    let error = run_reopen(request()).unwrap_err();
    assert!(format!("{error:#}").contains(FAILURE), "{error:#}");
    let child = rx
        .try_recv()
        .expect("launch-failure hook must report its session before reopen returns");
    for legacy in [false, true] {
        let store = Store::open_unchecked(&child);
        if legacy {
            store.record(CoreRecord::Launch).remove().unwrap();
        }
        let barrier = Arc::new(Barrier::new(2));
        let ready = barrier.clone();
        let owned = child.clone();
        let worker = thread::spawn(move || {
            ready.wait();
            launch::spawn(&Store::open_unchecked(&owned), &mut command(), |_| {
                panic!("fenced spawn created a process")
            })
            .unwrap_err()
        });
        barrier.wait();
        assert!(format!("{:#}", worker.join().unwrap()).contains("cancelled"));
        assert!(!child.join(PROVIDER_PROCESS_FILE).exists());
    }
}

#[test]
fn concurrent_reopens_reserve_safe_generic_failure_once() {
    let root = tempfile::tempdir().unwrap();
    let (source, child, _) = interrupted(root.path());
    update_status(&child, SessionState::Failed, None, None).unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let source = source.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                Reservation::claim(&source, SOURCE, REOPEN_TEST_CONVERSATION)
            })
        })
        .collect();
    barrier.wait();
    let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
}

#[test]
fn failure_before_durable_launch_receipt_retains_reopen_but_preserves_fresh_drop() {
    if isolated(concat!(
        module_path!(),
        "::failure_before_durable_launch_receipt_retains_reopen_but_preserves_fresh_drop"
    )) {
        return;
    }
    let root = Reader::state_root().unwrap();
    let mut resolved = start_source_fixture(&root);
    resolved.start.spec_mut().yolo = true;
    let attempt = resolved.create().unwrap();
    let source = attempt.source_directory.clone();
    let created = &attempt.prepared.created;
    let child = created.directory.clone();
    let child_id = created.id.clone();
    assert_eq!(source, root.join(&attempt.source_id));
    assert_eq!(child, root.join(&child_id));
    fs::create_dir(child.join(launch::FILE)).unwrap();

    // Fresh keeps the ordinary Claim Drop behavior, even before launch::begin wrote
    // a receipt. Exercise the complete Initial prompt operation, not its error hook.
    let mut fresh_report = TurnReport::new(false, serde_json::Map::new());
    let fresh_error = start_operation()
        .run(
            &attempt.prepared,
            &session_start::FreshSource,
            &mut fresh_report,
        )
        .unwrap_err();
    let original = format!("{fresh_error:#}");
    assert!(fresh_report.address().is_some());
    assert!(!child.join(TURN_CLAIM_FILE).exists());
    assert!(!child.join(REOPEN_REFUSAL_FILE).exists());
    assert_eq!(
        Reader::open_unchecked(&child).status().unwrap().state,
        SessionState::Launching
    );

    // Reopen fences that same stage and settles its marker without requiring the caller
    // to run a separate finish step. Missing durable launch evidence keeps the claim.
    let mut report = TurnReport::new(false, serde_json::Map::new());
    let (outcome, gate) = attempt.run(&start_operation(), &mut report);
    let error = outcome.unwrap_err();
    assert_eq!(gate, Some(ReopenGate::LaunchFailed));
    assert_eq!(reopen_refusal_gate(&error), Some(ReopenGate::LaunchFailed));
    assert!(format!("{error:#}").contains(&original), "{error:#}");
    let (_, request_id) = report.address().unwrap();
    assert_ne!(
        Some(request_id),
        fresh_report.address().map(|(_, request)| request)
    );
    let refusal = read_reopen_launch_refusal(&child).unwrap();
    let token = turn::current_claim_token(&Reader::open_unchecked(&child))
        .unwrap()
        .unwrap();
    assert_eq!(refusal.schema, 3);
    assert_eq!(refusal.phase, "launch");
    assert_eq!(refusal.gate, "launch-failed");
    assert_eq!(refusal.claim_token.as_deref(), Some(token.as_str()));
    assert_eq!(refusal.detail, original);
    let settlement_error = error.to_string();
    assert!(
        settlement_error.contains("refusing non-regular session file")
            && settlement_error.contains(child.join(launch::FILE).to_str().unwrap()),
        "settlement must read the actual child's refused launch receipt: {settlement_error}"
    );
    assert!(source.join(REOPEN_MARKER_FILE).exists());
    assert!(child.join(TURN_CLAIM_FILE).exists());
    assert_eq!(
        Reader::open_unchecked(&child).status().unwrap().state,
        SessionState::Failed
    );
}

#[test]
fn untrusted_receipts_and_later_claims_retain_even_with_a_gone_provider() {
    let root = tempfile::tempdir().unwrap();
    let (source, child, token) = interrupted(root.path());
    let store = Store::open_unchecked(&child);
    update_status(&child, SessionState::Failed, None, None).unwrap();
    let mut standin = StandIn(command().spawn().unwrap());
    record_provider_process(&child, CHILD, &standin.0).unwrap();
    drop(standin.0.stdin.take());
    standin.0.wait().unwrap();
    let receipt = json!({"schema":1,"claim_token":token,"deadline_unix_ms":1,"phase":"pending"});
    for wire in [
        {
            let mut r = receipt.clone();
            r["schema"] = json!(2);
            r
        },
        {
            let mut r = receipt.clone();
            r["claim_token"] = json!("different");
            r
        },
        {
            let mut r = receipt.clone();
            r["phase"] = json!("invalid");
            r
        },
    ] {
        store.record(CoreRecord::Launch).write_json(&wire).unwrap();
        assert!(inspect_reopen_source(&source, SOURCE).is_err());
    }
    store
        .record(CoreRecord::Launch)
        .write_json(&receipt)
        .unwrap();
    fs::write(child.join(TURN_CLAIM_FILE), b"later-turn").unwrap();
    assert!(inspect_reopen_source(&source, SOURCE).is_err());
    fs::write(child.join(TURN_CLAIM_FILE), token.as_bytes()).unwrap();
    assert!(inspect_reopen_source(&source, SOURCE).is_ok());
}
