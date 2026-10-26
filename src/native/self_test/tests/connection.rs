//! Producer to self-test: a real Delivery or deadline error through the real addressed JSON
//! output of ask and tell, parsed as `Installed` parses a child's stdout, then classified.
//! The child run prints the bytes and checks the Claim; the parent parses and classifies.
use super::super::{Outcome, failure, legacy_failure, reply_from_output, step};
use crate::native::delivery::test_support::Fixture;
use crate::native::delivery::{Turn, Undelivered};
use crate::native::session::primitives::STATE_DIR_ENV;
use crate::native::session::{RecordStore, turn};
use crate::native::session_start::initial_delivery_failure;
use crate::native::turn_report::TurnReport;
use crate::native::turn_run::{Output, SessionFacts, Timing, TurnRun, run_with};
use agent_bridge::FirstPartyCli;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

const CHILD: &str = "AGENT_BRIDGE_SELF_TEST_CONNECTION";

struct Case {
    label: &'static str,
    initial: bool,
    surface: bool,
    result: &'static str,
    fail: &'static str,
}

const CASES: &[Case] = &[
    Case {
        label: "FOLLOW_NATIVE_UNCERTAIN",
        initial: false,
        surface: false,
        result: "uncertain",
        fail: "",
    },
    Case {
        label: "FOLLOW_SURFACE_UNCERTAIN",
        initial: false,
        surface: true,
        result: "uncertain",
        fail: "",
    },
    Case {
        label: "INITIAL_UNCERTAIN",
        initial: true,
        surface: true,
        result: "uncertain",
        fail: "",
    },
    Case {
        label: "FOLLOW_TIMEOUT",
        initial: false,
        surface: true,
        result: "sent",
        fail: "timeout",
    },
    Case {
        label: "FOLLOW_NOT_SENT",
        initial: false,
        surface: true,
        result: "not sent",
        fail: "",
    },
    Case {
        label: "INITIAL_NOT_SENT",
        initial: true,
        surface: true,
        result: "not sent",
        fail: "",
    },
];

// One ask or tell through the Turn run with the production hooks, then the report's output.
fn run_case(root: &Path, case: &Case) {
    let mut f = Fixture::in_state_root(root);
    f.fake.surface = case.surface;
    f.fake.result = case.result;
    f.checks.fail = case.fail;
    let id = f.session_id();
    let (claim, baseline) = if case.initial {
        (f.claim(true), 0)
    } else {
        f.claim_ready()
    };
    let extra = Map::new();
    let run = TurnRun {
        facts: SessionFacts {
            id: &id,
            directory: f.store.directory(),
            provider: FirstPartyCli::Pi,
            provider_path: Path::new("provider"),
            surface: &f.surface,
        },
        timing: Timing {
            deadline: Instant::now() + Duration::from_secs(30),
            requested: Duration::from_secs(30),
        },
        output: Output {
            detach: true,
            json: true,
        },
        extra: &extra,
        baseline,
    };
    let mut report = TurnReport::new(true, Map::from_iter([("session".into(), json!(id))]));
    let mut gate = |_| Ok(());
    println!("{}_ID {id}", case.label);
    println!("{}_BEGIN", case.label);
    let outcome = if case.initial {
        // ask's and reopen's own hook (session_start::Start::run).
        let hook = |undelivered: Undelivered| {
            initial_delivery_failure(
                undelivered.error,
                undelivered.uncertain,
                FirstPartyCli::Pi,
                &id,
            )
        };
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
        // tell's own hook.
        let hook = |undelivered: Undelivered| undelivered.error;
        run_with(
            &mut report,
            claim,
            Turn::FollowUp { prompt: "prompt" },
            &run,
            hook,
            &f.fake,
            &f.checks,
        )
    };
    assert!(report.finish(outcome, Map::new()).is_err());
    println!("{}_END", case.label);
    // The hint changes no lifecycle: one send at most, never a resend, and the Claim
    // retained exactly when the prompt may have been delivered.
    let sends = f.calls().iter().filter(|call| **call == "send").count();
    assert_eq!(sends, usize::from(case.fail.is_empty()), "{}", case.label);
    assert_eq!(
        turn::current_claim_token(&f.store).unwrap().is_some(),
        case.result == "uncertain",
        "{}",
        case.label
    );
    if case.result == "uncertain" {
        assert!(!f.calls().iter().any(|call| call.starts_with("discard")));
    }
}

#[test]
fn real_delivery_errors_reach_self_test_through_their_json_output() {
    if std::env::var_os(CHILD).is_some() {
        let root = std::env::var_os(STATE_DIR_ENV).unwrap();
        for case in CASES {
            run_case(Path::new(&root), case);
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    RecordStore::at(root.path())
        .set_directory_private()
        .unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native::self_test::tests::connection::real_delivery_errors_reach_self_test_through_their_json_output",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env(STATE_DIR_ENV, root.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let section = |label: &str| {
        text.split(&format!("{label}_BEGIN\n"))
            .nth(1)
            .unwrap()
            .split(&format!("{label}_END\n"))
            .next()
            .unwrap()
            .to_owned()
    };
    let id = |label: &str| {
        text.split(&format!("{label}_ID "))
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned()
    };
    // (hint, error, outcome, wording outcome with the recorded state, wording outcome when
    // the record read fails and the addressed output falls back to request_state "unknown")
    let expected = |label: &str| -> (Option<&str>, String, Outcome, Outcome, Outcome) {
        let session = id(label);
        let typed = format!(
            "provider follow-up transport fake failed: failed to type into visible Windows Console session {session}: send cause"
        );
        let initial =
            format!("failed to deliver the initial prompt to pi session {session}: send cause");
        match label {
            "FOLLOW_NATIVE_UNCERTAIN" => (
                Some("delivery_uncertain"),
                "provider follow-up transport fake could not confirm delivery; the turn remains claimed until the target reports completion or the session is explicitly closed: send cause".to_owned(),
                Outcome::NotVerified,
                Outcome::NotVerified,
                Outcome::NotVerified,
            ),
            // The declared corrections. The wording reached NotVerified only through the
            // retained Claim's pending state; without that read it was Failed.
            "FOLLOW_SURFACE_UNCERTAIN" => (
                Some("delivery_uncertain"),
                typed,
                Outcome::NotVerified,
                Outcome::NotVerified,
                Outcome::Failed,
            ),
            "INITIAL_UNCERTAIN" => (
                Some("delivery_uncertain"),
                initial,
                Outcome::NotVerified,
                Outcome::NotVerified,
                Outcome::Failed,
            ),
            "FOLLOW_TIMEOUT" => (
                Some("timed_out"),
                "provider follow-up transport fake exhausted its total timeout before delivery: timed out after 30 seconds".to_owned(),
                Outcome::TimedOut,
                Outcome::TimedOut,
                Outcome::TimedOut,
            ),
            "FOLLOW_NOT_SENT" => (None, typed, Outcome::Failed, Outcome::Failed, Outcome::Failed),
            "INITIAL_NOT_SENT" => (None, initial, Outcome::Failed, Outcome::Failed, Outcome::Failed),
            _ => unreachable!(),
        }
    };
    for case in CASES {
        let bytes = section(case.label);
        let (kind, error, outcome, legacy, unread) = expected(case.label);
        let value: Value = serde_json::from_str(&bytes).unwrap();
        assert_eq!(value["failure_kind"].as_str(), kind, "{}", case.label);
        assert_eq!(value["error"], error, "{}", case.label);
        assert_eq!(value["ok"], false);
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["session"], id(case.label));
        // The addressed output read the recorded request through the state root.
        assert!(
            value["request_id"]
                .as_str()
                .unwrap()
                .starts_with("request-")
        );
        assert_ne!(value["request_state"], "unknown", "{}", case.label);
        assert!(value.get("timed_out").is_none());
        // Exactly what Installed builds from a failed child's stdout.
        let reply = reply_from_output(false, bytes.as_bytes(), b"ignored stderr");
        assert_eq!(failure(&reply), (outcome, error.clone()), "{}", case.label);
        assert_eq!(
            legacy_failure(&reply),
            (legacy, error.clone()),
            "{}",
            case.label
        );
        assert_eq!(
            value["request_state"] == "pending",
            case.result == "uncertain",
            "{}",
            case.label
        );
        // The same output had the record read failed: only the hint keeps the class.
        let mut fallback = value.clone();
        fallback["request_state"] = json!("unknown");
        let reply_unread = reply_from_output(
            false,
            serde_json::to_string_pretty(&fallback).unwrap().as_bytes(),
            b"",
        );
        assert_eq!(failure(&reply_unread).0, outcome, "{}", case.label);
        assert_eq!(legacy_failure(&reply_unread).0, unread, "{}", case.label);
        let step = step("tell", Instant::now(), &reply);
        assert_eq!(step.outcome, outcome);
        assert_eq!(
            step.request_address,
            Some(format!(
                "{}/{}",
                id(case.label),
                value["request_id"].as_str().unwrap()
            ))
        );
    }
}
