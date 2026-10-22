//! The Turn run at its own interface: a real session directory and Claim, the fake
//! Transport and fake Checks of Delivery. Delivery's settlement is proven in
//! `delivery/tests.rs`; these tests prove what the run adds: the report address, whether
//! the wait and the emission happen, the delivery-error hook selection and the extras.
use super::super::delivery::test_support::Fixture;
use super::super::reopen::{
    ReopenGate, ResumedFrom, refuse_follow_up_to_shared_conversation, reopen_refusal_gate,
};
use super::super::session::{CoreRecord, Store, hold};
use super::*;
use crate::native::session::{Reader, SessionState, turn};
use crate::native::turn_report::TurnReport;
use agent_bridge::{FirstPartyCli, PUBLIC_COMMAND};
use anyhow::Result;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

const SESSION: &str = "session-test";

fn report() -> TurnReport {
    TurnReport::new(true, serde_json::Map::new())
}

fn run_fixture<'a>(
    f: &'a Fixture,
    timing: Timing,
    output: Output,
    extra: &'a ResultExtra,
    baseline: usize,
) -> TurnRun<'a> {
    TurnRun {
        facts: SessionFacts {
            id: SESSION,
            directory: f.store.directory(),
            provider: FirstPartyCli::Pi,
            provider_path: Path::new("provider"),
            surface: &f.surface,
        },
        timing,
        output,
        extra,
        baseline,
    }
}

fn timing() -> Timing {
    Timing {
        deadline: Instant::now() + Duration::from_secs(30),
        requested: Duration::from_secs(30),
    }
}

fn waiting_json() -> Output {
    Output {
        detach: false,
        json: true,
    }
}

/// Runs this test binary again with `child` set and returns the labelled stdout sections.
fn capture(test: &str, child: &str) -> BTreeMap<String, String> {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("native::turn_run::tests::{test}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(child, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let mut sections = BTreeMap::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("SECTION_BEGIN ") {
        let after = &rest[start + "SECTION_BEGIN ".len()..];
        let (label, body) = after.split_once('\n').unwrap();
        let (body, tail) = body.split_once("SECTION_END\n").unwrap();
        sections.insert(label.to_owned(), body.to_owned());
        rest = tail;
    }
    sections
}

fn section_json(sections: &BTreeMap<String, String>, label: &str) -> serde_json::Value {
    serde_json::from_str(&sections[label]).unwrap()
}

fn resumed_from() -> ResumedFrom {
    ResumedFrom {
        session: "session-source".into(),
        provider_session_id: "conversation-1".into(),
        event_id: "event-1-1.json".into(),
    }
}

fn reopen_extra() -> ResultExtra {
    let mut extra = ResultExtra::new();
    extra.insert(
        "source_session".to_owned(),
        serde_json::Value::String("session-source".into()),
    );
    extra.insert(
        "resumed_from".to_owned(),
        serde_json::to_value(resumed_from()).unwrap(),
    );
    extra
}

/// Publishes a result for the Claim under `turn_id` when the fake sends.
fn publish_on_send(f: &Fixture, token: String, message: &'static str, turn_id: &'static str) {
    let dir = f.store.directory().to_owned();
    *f.fake.on_send.borrow_mut() = Some(Box::new(move || {
        turn::Report::for_claim(
            &Store::open_unchecked(&dir),
            FirstPartyCli::Pi,
            Some(&token),
        )
        .complete(message, Some("pi-session".into()), Some(turn_id.into()))
        .unwrap();
    }));
}

/// A follow-up Claim taken after one delivered and completed turn, so the baseline is
/// nonzero and an old event cannot satisfy a miswired wait. The earlier turn has its own
/// turn id: a completion repeating an earlier event's provider session and turn id is
/// dropped as a duplicate.
fn claim_after_one_completed_turn(f: &Fixture) -> (turn::Claim, usize) {
    let (earlier, _) = f.claim_ready();
    publish_on_send(f, earlier.token().to_owned(), "earlier", "earlier-turn");
    f.follow(earlier).unwrap();
    f.fake.calls.borrow_mut().clear();
    let (claim, baseline) = f.claim_ready();
    assert_eq!(baseline, 1);
    (claim, baseline)
}

#[test]
fn detached_json_follows_delivery_settlement() {
    const CHILD: &str = "AGENT_BRIDGE_TURN_RUN_DETACHED_FIXTURE";
    if std::env::var_os(CHILD).is_some() {
        for initial in [true, false] {
            let f = Fixture::new();
            let (claim, baseline) = if initial {
                (f.claim(true), 0)
            } else {
                f.claim_ready()
            };
            let request_id = claim.receipt().request_id.clone();
            let extra = ResultExtra::new();
            let run = run_fixture(
                &f,
                timing(),
                Output {
                    detach: true,
                    json: true,
                },
                &extra,
                baseline,
            );
            let mut report = report();
            let mut gate = |_| Ok(());
            println!(
                "SECTION_BEGIN {}",
                if initial { "initial" } else { "follow" }
            );
            if initial {
                run_with(
                    &mut report,
                    claim,
                    Turn::Initial { gate: &mut gate },
                    &run,
                    |_| unreachable!("delivery succeeded"),
                    &f.fake,
                    &f.checks,
                )
            } else {
                run_with(
                    &mut report,
                    claim,
                    Turn::FollowUp { prompt: "prompt" },
                    &run,
                    |_| unreachable!("delivery succeeded"),
                    &f.fake,
                    &f.checks,
                )
            }
            .unwrap();
            println!("SECTION_END");
            assert_eq!(report.address(), Some((SESSION, request_id.as_str())));
            assert_eq!(f.store.status().unwrap().state, SessionState::Working);
            assert!(turn::current_claim_token(&f.store).unwrap().is_some());
            assert!(f.calls().contains(&"send"));
        }
        return;
    }
    let sections = capture("detached_json_follows_delivery_settlement", CHILD);
    assert_eq!(sections.len(), 2);
    for label in ["initial", "follow"] {
        let value = section_json(&sections, label);
        assert_eq!(value["ok"], true);
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["request_state"], "accepted");
        assert_eq!(value["session"], SESSION);
        assert!(
            value["request_id"]
                .as_str()
                .unwrap()
                .starts_with("request-")
        );
        for field in ["result", "turn_id", "provider_session_id"] {
            assert!(value[field].is_null());
        }
        assert_eq!(value["terminal"], "windows-console");
        assert_eq!(value["provider"], "pi");
    }
}

#[test]
fn completed_result_is_waited_for_past_the_claim_ready_baseline() {
    const CHILD: &str = "AGENT_BRIDGE_TURN_RUN_COMPLETED_FIXTURE";
    if std::env::var_os(CHILD).is_some() {
        for (label, json, extra) in [
            ("completed", true, ResultExtra::new()),
            ("reopen", true, reopen_extra()),
            ("reopen_plain", false, reopen_extra()),
        ] {
            let f = Fixture::new();
            let (claim, baseline) = claim_after_one_completed_turn(&f);
            let request_id = claim.receipt().request_id.clone();
            publish_on_send(&f, claim.token().to_owned(), "done\nnext", "correlation");
            let run = run_fixture(
                &f,
                timing(),
                Output {
                    detach: false,
                    json,
                },
                &extra,
                baseline,
            );
            let mut report = report();
            println!("SECTION_BEGIN {label}");
            run_with(
                &mut report,
                claim,
                Turn::FollowUp { prompt: "prompt" },
                &run,
                |_| unreachable!("delivery succeeded"),
                &f.fake,
                &f.checks,
            )
            .unwrap();
            println!("SECTION_END");
            assert_eq!(report.address(), Some((SESSION, request_id.as_str())));
            assert!(turn::current_claim_token(&f.store).unwrap().is_none());
        }
        return;
    }
    let sections = capture(
        "completed_result_is_waited_for_past_the_claim_ready_baseline",
        CHILD,
    );
    assert_eq!(sections.len(), 3);
    let completed = section_json(&sections, "completed");
    assert_eq!(completed["ok"], true);
    assert_eq!(completed["request_state"], "completed");
    assert_eq!(completed["result"], "done\nnext");
    assert_eq!(completed["turn_id"], "correlation");
    assert_eq!(completed["provider_session_id"], "pi-session");
    assert_eq!(completed["session"], SESSION);
    assert!(completed.get("source_session").is_none());
    let reopen = section_json(&sections, "reopen");
    assert_eq!(reopen["request_state"], "completed");
    assert_eq!(reopen["result"], "done\nnext");
    assert_eq!(reopen["source_session"], "session-source");
    assert_eq!(
        reopen["resumed_from"],
        serde_json::to_value(resumed_from()).unwrap()
    );
    assert!(
        reopen["request_id"]
            .as_str()
            .unwrap()
            .starts_with("request-")
    );
    let plain = &sections["reopen_plain"];
    let request_id = plain
        .lines()
        .nth(1)
        .and_then(|line| line.strip_prefix("request: "))
        .unwrap();
    assert!(request_id.starts_with("request-"));
    assert_eq!(
        plain.as_str(),
        format!(
            "session: {SESSION}\nrequest: {request_id}\nsource_session: session-source\n\ndone\nnext\n"
        )
    );
}

#[test]
fn detached_reopen_extra_is_rendered_without_a_result() {
    const CHILD: &str = "AGENT_BRIDGE_TURN_RUN_DETACHED_REOPEN_FIXTURE";
    if std::env::var_os(CHILD).is_some() {
        let f = Fixture::new();
        let claim = f.claim(true);
        let extra = reopen_extra();
        let run = run_fixture(
            &f,
            timing(),
            Output {
                detach: true,
                json: true,
            },
            &extra,
            0,
        );
        let mut gate = |_| Ok(());
        println!("SECTION_BEGIN detached");
        run_with(
            &mut report(),
            claim,
            Turn::Initial { gate: &mut gate },
            &run,
            |_| unreachable!("delivery succeeded"),
            &f.fake,
            &f.checks,
        )
        .unwrap();
        println!("SECTION_END");
        return;
    }
    let sections = capture("detached_reopen_extra_is_rendered_without_a_result", CHILD);
    let value = section_json(&sections, "detached");
    assert_eq!(value["request_state"], "accepted");
    assert!(value["result"].is_null());
    assert_eq!(value["source_session"], "session-source");
    assert_eq!(
        value["resumed_from"],
        serde_json::to_value(resumed_from()).unwrap()
    );
}

#[test]
fn hold_written_after_the_claim_refuses_before_send_with_the_addressed_error() {
    let f = Fixture::new();
    let (claim, baseline) = f.claim_ready();
    let request_id = claim.receipt().request_id.clone();
    let dir = f.store.directory().to_owned();
    *f.checks.before_begin.borrow_mut() = Some(Box::new(move || {
        hold::change(&Store::open_unchecked(&dir), false).unwrap();
    }));
    let extra = ResultExtra::new();
    let run = run_fixture(&f, timing(), waiting_json(), &extra, baseline);
    let mut report = report();
    let hooked = RefCell::new(Vec::new());
    let error = run_with(
        &mut report,
        claim,
        Turn::FollowUp { prompt: "prompt" },
        &run,
        |error| {
            hooked.borrow_mut().push(format!("{error:#}"));
            error
        },
        &f.fake,
        &f.checks,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("is held"), "{error:#}");
    assert!(!f.calls().contains(&"send"));
    assert_eq!(hooked.borrow().as_slice(), [format!("{error:#}")]);
    assert_eq!(report.address(), Some((SESSION, request_id.as_str())));
    assert!(hold::read(&Reader::open_unchecked(f.store.directory())).unwrap());
    assert!(turn::current_claim_token(&f.store).unwrap().is_none());
}

#[test]
fn result_wait_timeout_keeps_the_inspect_context_and_skips_the_delivery_hook() {
    let f = Fixture::new();
    let (claim, baseline) = f.claim_ready();
    let extra = ResultExtra::new();
    let run = run_fixture(
        &f,
        Timing {
            deadline: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            requested: Duration::from_secs(7),
        },
        waiting_json(),
        &extra,
        baseline,
    );
    let error = run_with(
        &mut report(),
        claim,
        Turn::FollowUp { prompt: "prompt" },
        &run,
        |_| unreachable!("the delivery hook must not see a result timeout"),
        &f.fake,
        &f.checks,
    )
    .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        format!(
            "session {SESSION} in Windows Console did not return a successful result; use `{PUBLIC_COMMAND} inspect {SESSION}` to inspect its recorded state: timed out after 7 seconds"
        )
    );
    assert!(f.calls().contains(&"send"));
    assert_eq!(f.store.status().unwrap().state, SessionState::Working);
    assert!(turn::current_claim_token(&f.store).unwrap().is_some());
    assert!(
        f.store
            .record(CoreRecord::Events)
            .path()
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn result_failure_keeps_the_inspect_context_and_skips_the_delivery_hook() {
    let f = Fixture::new();
    let (claim, baseline) = claim_after_one_completed_turn(&f);
    let token = claim.token().to_owned();
    let dir = f.store.directory().to_owned();
    *f.fake.on_send.borrow_mut() = Some(Box::new(move || {
        turn::Report::for_claim(
            &Store::open_unchecked(&dir),
            FirstPartyCli::Pi,
            Some(&token),
        )
        .fail("provider gave up", None, Some("correlation".into()))
        .unwrap();
    }));
    let extra = ResultExtra::new();
    let run = run_fixture(&f, timing(), waiting_json(), &extra, baseline);
    let error = run_with(
        &mut report(),
        claim,
        Turn::FollowUp { prompt: "prompt" },
        &run,
        |_| unreachable!("the delivery hook must not see a result failure"),
        &f.fake,
        &f.checks,
    )
    .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        format!(
            "session {SESSION} in Windows Console did not return a successful result; use `{PUBLIC_COMMAND} inspect {SESSION}` to inspect its recorded state: provider gave up"
        )
    );
}

#[test]
fn delivery_error_hook_receives_the_unwrapped_error_once() {
    #[derive(Debug)]
    struct Hooked;
    impl std::fmt::Display for Hooked {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("hooked")
        }
    }
    impl std::error::Error for Hooked {}
    for (initial, result) in [
        (false, "not sent"),
        (false, "uncertain"),
        (true, "not sent"),
    ] {
        let mut f = Fixture::new();
        f.fake.result = result;
        let (claim, baseline) = if initial {
            (f.claim(true), 0)
        } else {
            f.claim_ready()
        };
        let request_id = claim.receipt().request_id.clone();
        let extra = ResultExtra::new();
        let run = run_fixture(&f, timing(), waiting_json(), &extra, baseline);
        let seen = RefCell::new(Vec::new());
        let hook = |error: anyhow::Error| {
            seen.borrow_mut().push(format!("{error:#}"));
            error.context(Hooked)
        };
        let mut report = report();
        let mut gate = |_| Ok(());
        let error = if initial {
            run_with(
                &mut report,
                claim,
                Turn::Initial { gate: &mut gate },
                &run,
                hook,
                &f.fake,
                &f.checks,
            )
        } else {
            run_with(
                &mut report,
                claim,
                Turn::FollowUp { prompt: "prompt" },
                &run,
                hook,
                &f.fake,
                &f.checks,
            )
        }
        .unwrap_err();
        // Delivery's own wording, settled before the hook sees it (delivery/tests.rs pins it).
        let expected = if initial {
            "send cause"
        } else {
            "provider follow-up transport fake failed: failed to type into visible Windows Console session session-test: send cause"
        };
        assert_eq!(seen.borrow().as_slice(), [expected], "{initial} {result}");
        assert!(error.downcast_ref::<Hooked>().is_some());
        assert_eq!(format!("{error:#}"), format!("hooked: {expected}"));
        assert_eq!(report.address(), Some((SESSION, request_id.as_str())));
        assert_eq!(
            turn::current_claim_token(&f.store).unwrap().is_some(),
            result == "uncertain",
            "{initial} {result}"
        );
    }
}

// The caller boundary tell keeps outside the Turn run: the report is addressed right
// after the Claim, so a refusal of the follow-up to a reopened conversation still
// carries the request id, and `run_tell` reads the gate from the typed refusal.
#[test]
fn tell_refusal_after_the_claim_leaves_the_request_id_and_gate_on_the_report() {
    let f = Fixture::new();
    let (mut claim, _) = f.claim_ready();
    let mut report = TurnReport::new(
        true,
        serde_json::Map::from_iter([("session".into(), serde_json::json!(SESSION))]),
    );
    let request_id = claim.receipt().request_id.clone();
    report.addressed(SESSION.to_owned(), request_id.clone());
    let resumed_from = resumed_from();
    let outcome: Result<()> = refuse_follow_up_to_shared_conversation(
        FirstPartyCli::Pi,
        f.store.directory(),
        SESSION,
        Some(&resumed_from),
        Instant::now() + Duration::from_secs(5),
        &mut claim,
        &SessionState::Ready,
    );
    drop(claim);
    let gate = outcome.as_ref().err().and_then(reopen_refusal_gate);
    assert_eq!(gate, Some(ReopenGate::ReopenVerificationFailed));
    assert_eq!(report.address(), Some((SESSION, request_id.as_str())));
    assert!(format!("{:#}", outcome.unwrap_err()).starts_with(&format!(
        "follow-up to reopened session {SESSION} was refused before delivery"
    )));
    assert!(!f.calls().contains(&"send"));
    assert!(turn::current_claim_token(&f.store).unwrap().is_none());
    assert_eq!(f.store.status().unwrap().state, SessionState::Ready);
}
