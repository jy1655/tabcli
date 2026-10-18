use super::launch_failure::isolated;
use super::*;
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
    claim_reopen_marker(&source, SOURCE, REOPEN_TEST_CONVERSATION)
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
        Some("already-reopened")
    );
    assert_eq!(snapshot_directory(&child), changed);
    fs::write(child.join(TURN_CLAIM_FILE), token.as_bytes()).unwrap();
    fail_next();
    let error = run_reopen(request()).unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("launch-failed"),
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
fn failed_refusal_write_keeps_typed_gate_and_marker() {
    if isolated(concat!(
        module_path!(),
        "::failed_refusal_write_keeps_typed_gate_and_marker"
    )) {
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
        Some("launch-failed"),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains(FAILURE));
    assert!(format!("{error:#}").contains("refusal"));
    assert!(source.join(REOPEN_MARKER_FILE).exists());
    assert_eq!(
        reopen_refusal_gate(&run_reopen(request()).unwrap_err()),
        Some("already-reopened")
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
            Some("already-reopened"),
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
            REOPEN_LAUNCH_GATE,
            "specific wrapper refusal".into(),
        );
        anyhow::anyhow!(FAILURE)
    });
    let error = run_reopen(request()).unwrap_err();
    assert_eq!(reopen_refusal_gate(&error), Some(REOPEN_LAUNCH_GATE));
    assert!(format!("{error:#}").contains(FAILURE));
    let child = created(&root);
    assert_eq!(read_reopen_launch_refusal(&child).unwrap().schema, 2);
    assert!(!source.join(REOPEN_MARKER_FILE).exists());
    let marker = claim_reopen_marker(&source, SOURCE, REOPEN_TEST_CONVERSATION).unwrap();
    marker
        .finalize(child.file_name().unwrap().to_str().unwrap())
        .unwrap();
    Store::open_unchecked(&child)
        .record(CoreRecord::ReopenRefusal)
        .write_json(&refusal("token"))
        .unwrap();
    record_reopen_refusal(&child, REOPEN_LAUNCH_GATE, "late wrapper refusal".into());
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
        assert_eq!(reopen_refusal_gate(&error), Some("launch-failed"));
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
        let next = claim_reopen_marker(&source, SOURCE, REOPEN_TEST_CONVERSATION).unwrap();
        drop(next);
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
    run_reopen(request()).unwrap_err();
    let child = rx.recv().unwrap();
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
                claim_reopen_marker(&source, SOURCE, REOPEN_TEST_CONVERSATION)
            })
        })
        .collect();
    barrier.wait();
    let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
}
