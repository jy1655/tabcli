//! Characterization shared by the three real host entry points. The script runs only
//! at read boundaries on the calling test thread; no scheduler or wall-clock wait.
use super::*;
use std::cell::RefCell;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum Point {
    InitialReceipt,
    BudgetCaptured,
    BeforeBinding,
    Retry,
}

type Hook = Box<dyn FnMut(Point, &mut Instant)>;
struct Script {
    now: Instant,
    hook: Hook,
    retries: usize,
    points: Vec<Point>,
}

thread_local! {
    static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
}

pub(super) fn now() -> Option<Instant> {
    SCRIPT.with_borrow(|script| script.as_ref().map(|script| script.now))
}

pub(in crate::native) fn checkpoint(point: Point) {
    SCRIPT.with_borrow_mut(|script| {
        if let Some(script) = script {
            script.points.push(point);
            (script.hook)(point, &mut script.now);
        }
    });
}

pub(super) fn retry(delay: Duration) -> bool {
    SCRIPT.with_borrow_mut(|script| {
        let Some(script) = script else { return false };
        script.retries += 1;
        script.points.push(Point::Retry);
        assert!(
            script.retries <= 2,
            "binding wait did not settle at the scripted retry"
        );
        // The real wait sleeps a full 10 ms, even with less than 10 ms remaining.
        assert_eq!(delay, Duration::from_millis(10));
        script.now += delay;
        (script.hook)(Point::Retry, &mut script.now);
        true
    })
}

struct Reset(Option<u128>);
impl Drop for Reset {
    fn drop(&mut self) {
        SCRIPT.set(None);
        session::tests::FIXED_UNIX_MS.set(self.0);
    }
}

fn receipt(directory: &Path, change: impl FnOnce(&mut Record)) {
    let store = Store::open_unchecked(directory);
    let mut record = read(&store).unwrap().unwrap();
    change(&mut record);
    store
        .record(CoreRecord::Launch)
        .write_json(&record)
        .unwrap();
}

fn corrupt_claim(directory: &Path) {
    let path = directory.join(TURN_CLAIM_FILE);
    fs::remove_file(&path).unwrap();
    fs::create_dir(path).unwrap();
}

pub(in crate::native) fn characterize(
    name: &'static str,
    wait: impl Fn(&Path, &str) -> Result<()>,
) {
    let cancelled = if name == "Terminal.app" {
        "Terminal.app launch was cancelled or timed out before its surface was bound".to_owned()
    } else {
        format!("{name} launch was cancelled or timed out before surface binding")
    };
    for case in [
        "bound",
        "legacy-owner",
        "missing-initial",
        "invalid-initial",
        "initial-identity",
        "initial-replaced",
        "retry-replaced",
        "retry-cancelled",
        "retry-released",
        "retry-spawning",
        "receipt-before-status",
        "missing-retry",
        "expired-before-bad-status",
        "cancelled-before-bad-claim",
        "phase-before-bad-claim",
        "token-before-bad-claim",
        "claim-before-bad-binding",
        "invalid-binding",
        "owner-before-surface",
        "late-cancellation",
        "wall-expired",
        "rollback",
        "extended-deadline",
        "short-budget",
        "expired-then-rollback",
        "thirty-second-cap",
    ] {
        let _reset = Reset(session::tests::FIXED_UNIX_MS.replace(Some(1_000)));
        let fixture = tempfile::Builder::new()
            .prefix("session-binding-")
            .tempdir()
            .unwrap();
        let directory = fixture.path();
        fs::create_dir(directory.join("events")).unwrap();
        update_status(directory, SessionState::Launching, None, None).unwrap();
        let claim = acquire_turn_claim(directory).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        let store = Store::open_unchecked(directory);
        store
            .record(CoreRecord::Launch)
            .write_json(&Record {
                schema: 1,
                claim_token: token.clone(),
                deadline_unix_ms: 1_100,
                phase: Phase::Pending,
            })
            .unwrap();
        let id = directory.file_name().unwrap().to_str().unwrap();
        let binding = serde_json::json!({
            "terminal": match name { "iTerm2" => "iterm2", "Terminal.app" => "apple-terminal", _ => "wezterm" },
            "session_id": "host-id", "window_id": "window", "managed_session_id": id,
            "wezterm_mux": { "socket": "/tmp/binding-socket", "pid": 42,
                "start_seconds": 1, "start_microseconds": 0, "owns_gui": false },
        });
        let mut expected = cancelled.clone();
        let success = matches!(case, "bound" | "late-cancellation")
            || (case == "legacy-owner" && name == "iTerm2")
            || (matches!(
                case,
                "rollback" | "extended-deadline" | "short-budget" | "expired-then-rollback"
            ) && name == "Terminal.app");

        match case {
            "missing-initial" => {
                fs::remove_file(directory.join(FILE)).unwrap();
                expected = format!("missing {name} launch receipt");
            }
            "invalid-initial" => {
                fs::write(directory.join(FILE), b"{").unwrap();
                expected = "invalid launch receipt".into();
            }
            "initial-identity" => {
                receipt(directory, |r| r.schema = 2);
                expected = "invalid launch receipt identity".into();
            }
            "legacy-owner" => {
                let mut binding = binding.clone();
                binding
                    .as_object_mut()
                    .unwrap()
                    .remove("managed_session_id");
                store
                    .record(CoreRecord::Terminal)
                    .write_json(&binding)
                    .unwrap();
                expected = "terminal handle is missing its managed session binding".into();
            }
            "bound" | "late-cancellation" => store
                .record(CoreRecord::Terminal)
                .write_json(&binding)
                .unwrap(),
            "short-budget" => receipt(directory, |r| r.deadline_unix_ms = 1_005),
            "expired-then-rollback" => receipt(directory, |r| r.deadline_unix_ms = 999),
            "thirty-second-cap" => receipt(directory, |r| r.deadline_unix_ms = 100_000),
            "owner-before-surface" => {
                let mut binding = binding.clone();
                binding["managed_session_id"] = "session-foreign".into();
                binding["session_id"] = "wrong-surface".into();
                store
                    .record(CoreRecord::Terminal)
                    .write_json(&binding)
                    .unwrap();
                expected =
                    format!("terminal handle belongs to managed session session-foreign, not {id}");
            }
            _ => {}
        }
        match case {
            "receipt-before-status" => expected = "invalid launch receipt".into(),
            "missing-retry" => expected = format!("missing {name} launch receipt"),
            "claim-before-bad-binding" => expected = "failed to inspect native turn claim".into(),
            "invalid-binding" => expected = format!("invalid {name} surface binding"),
            "expired-before-bad-status" => {
                // Capture the exact existing Reader error, including its path.
                let status = store.status().unwrap();
                fs::write(directory.join("status.json"), b"{").unwrap();
                expected = store.status().unwrap_err().to_string();
                store.write_status(&status).unwrap();
            }
            _ => {}
        }

        let path = directory.to_owned();
        let mut initial_seen = false;
        let mut retry_seen = false;
        SCRIPT.set(Some(Script {
            now: Instant::now(),
            retries: 0,
            points: Vec::new(),
            hook: Box::new(move |point, now| {
                if point == Point::InitialReceipt {
                    initial_seen = true;
                    if case == "initial-replaced" {
                        assert!(!path.join(TERMINAL_HANDLE_FILE).exists());
                        receipt(&path, |r| r.claim_token = "replacement".into());
                        fs::remove_file(path.join(TURN_CLAIM_FILE)).unwrap();
                        RecordStore::at(&path.join(TURN_CLAIM_FILE))
                            .write_private(b"replacement")
                            .unwrap();
                        Store::open_unchecked(&path)
                            .record(CoreRecord::Terminal)
                            .write_json(&binding)
                            .unwrap();
                    }
                }
                if point == Point::BudgetCaptured && case == "expired-then-rollback" {
                    session::tests::FIXED_UNIX_MS.set(Some(900));
                    Store::open_unchecked(&path)
                        .record(CoreRecord::Terminal)
                        .write_json(&binding)
                        .unwrap();
                }
                if point == Point::BeforeBinding && case == "late-cancellation" {
                    update_status(&path, SessionState::Closed, None, None).unwrap();
                }
                if point != Point::Retry {
                    return;
                }
                assert!(initial_seen, "retry before initial receipt");
                assert!(!retry_seen, "unexpected second retry for {case}");
                retry_seen = true;
                assert!(
                    !path.join(TERMINAL_HANDLE_FILE).exists(),
                    "retry must observe an absent binding"
                );
                Store::open_unchecked(&path)
                    .record(CoreRecord::Terminal)
                    .write_json(&binding)
                    .unwrap();
                match case {
                    "retry-replaced" => {
                        receipt(&path, |r| r.claim_token = "replacement".into());
                        fs::remove_file(path.join(TURN_CLAIM_FILE)).unwrap();
                        RecordStore::at(&path.join(TURN_CLAIM_FILE))
                            .write_private(b"replacement")
                            .unwrap();
                    }
                    "retry-cancelled" => {
                        update_status(&path, SessionState::Closed, None, None).unwrap()
                    }
                    "retry-released" => fs::remove_file(path.join(TURN_CLAIM_FILE)).unwrap(),
                    "retry-spawning" => receipt(&path, |r| r.phase = Phase::Spawning),
                    "receipt-before-status" => {
                        fs::write(path.join(FILE), b"{").unwrap();
                        fs::write(path.join("status.json"), b"{").unwrap();
                    }
                    "missing-retry" => {
                        fs::remove_file(path.join(FILE)).unwrap();
                        fs::write(path.join("status.json"), b"{").unwrap();
                    }
                    "expired-before-bad-status" => {
                        *now += Duration::from_secs(30);
                        receipt(&path, |r| r.deadline_unix_ms = 0);
                        fs::write(path.join("status.json"), b"{").unwrap();
                    }
                    "cancelled-before-bad-claim"
                    | "phase-before-bad-claim"
                    | "token-before-bad-claim"
                    | "claim-before-bad-binding" => {
                        match case {
                            "cancelled-before-bad-claim" => {
                                update_status(&path, SessionState::Closed, None, None).unwrap()
                            }
                            "phase-before-bad-claim" => {
                                receipt(&path, |r| r.phase = Phase::Spawning)
                            }
                            "token-before-bad-claim" => {
                                receipt(&path, |r| r.claim_token = "foreign".into())
                            }
                            _ => {}
                        }
                        corrupt_claim(&path);
                        fs::write(path.join(TERMINAL_HANDLE_FILE), b"{").unwrap();
                    }
                    "invalid-binding" => fs::write(path.join(TERMINAL_HANDLE_FILE), b"{").unwrap(),
                    "wall-expired" => session::tests::FIXED_UNIX_MS.set(Some(1_100)),
                    "rollback" => {
                        *now += Duration::from_millis(100);
                        session::tests::FIXED_UNIX_MS.set(Some(900));
                    }
                    "extended-deadline" => {
                        *now += Duration::from_millis(100);
                        receipt(&path, |r| r.deadline_unix_ms = 2_000);
                        session::tests::FIXED_UNIX_MS.set(Some(1_110));
                    }
                    "thirty-second-cap" => *now += Duration::from_secs(30),
                    "short-budget" => {}
                    _ => panic!("unexpected retry for {case}"),
                }
            }),
        }));
        let result = wait(directory, id);
        if success {
            result.unwrap_or_else(|error| panic!("{name}/{case}: {error:#}"));
        } else {
            assert_eq!(result.unwrap_err().to_string(), expected, "{name}/{case}");
        }
        SCRIPT.with_borrow(|script| {
            let script = script.as_ref().unwrap();
            if matches!(
                case,
                "missing-initial" | "invalid-initial" | "initial-identity"
            ) {
                assert!(
                    script.points.is_empty(),
                    "initial receipt must precede timer setup"
                );
            } else {
                assert_eq!(
                    &script.points[..2],
                    &[Point::InitialReceipt, Point::BudgetCaptured]
                );
            }
        });
        if case == "late-cancellation" {
            // Binding observation is not spawn authority: it deliberately has no final
            // recheck. The existing lock-protected spawn fence still refuses this state.
            let error = spawn(&store, &mut Command::new("must-not-spawn"), |_| {
                panic!("a cancelled host spawned a provider")
            })
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "provider launch cancelled: session is closed"
            );
        }
        if matches!(case, "initial-replaced" | "retry-replaced") {
            assert_eq!(read(&store).unwrap().unwrap().claim_token, "replacement");
            assert_eq!(
                turn::current_claim_token(&store).unwrap().as_deref(),
                Some("replacement")
            );
        }
        assert!(!directory.join(PROVIDER_PROCESS_FILE).exists());
        println!("{name}: {case} characterized");
    }
}
