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
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const CHILD: &str = "AGENT_BRIDGE_SELF_TEST_CONNECTION";
const DEADLINE: &str = "timed out after 30 seconds";

struct Case {
    label: &'static str,
    initial: bool,
    surface: bool,
    // The fake transport's send: "uncertain timeout" is uncertain with the production
    // remaining-time leaf as its cause.
    result: &'static str,
    fail: &'static str,
    // The addressed report cannot read the session's status, as when a record is missing.
    unread: bool,
    hint: Option<&'static str>,
    outcome: Outcome,
    // The wording classification of the same output, which self-test used before #204.
    legacy: Outcome,
}

impl Case {
    fn uncertain(&self) -> bool {
        self.result.starts_with("uncertain")
    }

    fn cause(&self) -> &'static str {
        if self.result == "uncertain timeout" {
            DEADLINE
        } else {
            "send cause"
        }
    }

    fn error(&self, session: &str) -> String {
        let cause = self.cause();
        if self.fail == "timeout" {
            format!(
                "provider follow-up transport fake exhausted its total timeout before delivery: {DEADLINE}"
            )
        } else if self.initial {
            format!("failed to deliver the initial prompt to pi session {session}: {cause}")
        } else if self.surface {
            format!(
                "provider follow-up transport fake failed: failed to type into visible Windows Console session {session}: {cause}"
            )
        } else {
            format!(
                "provider follow-up transport fake could not confirm delivery; the turn remains claimed until the target reports completion or the session is explicitly closed: {cause}"
            )
        }
    }
}

const fn case(
    label: &'static str,
    (initial, surface): (bool, bool),
    (result, fail, unread): (&'static str, &'static str, bool),
    hint: Option<&'static str>,
    outcome: Outcome,
    legacy: Outcome,
) -> Case {
    Case {
        label,
        initial,
        surface,
        result,
        fail,
        unread,
        hint,
        outcome,
        legacy,
    }
}

const NATIVE: (bool, bool) = (false, false);
const SURFACE: (bool, bool) = (false, true);
const INITIAL: (bool, bool) = (true, true);
const DU: Option<&str> = Some("delivery_uncertain");

const CASES: &[Case] = &[
    // Pending controls: the recorded request is read, and the retained Claim leaves it
    // pending, so the wording classification already reached NotVerified through it.
    case(
        "FOLLOW_NATIVE_UNCERTAIN",
        NATIVE,
        ("uncertain", "", false),
        DU,
        Outcome::NotVerified,
        Outcome::NotVerified,
    ),
    case(
        "FOLLOW_SURFACE_UNCERTAIN",
        SURFACE,
        ("uncertain", "", false),
        DU,
        Outcome::NotVerified,
        Outcome::NotVerified,
    ),
    case(
        "INITIAL_UNCERTAIN",
        INITIAL,
        ("uncertain", "", false),
        DU,
        Outcome::NotVerified,
        Outcome::NotVerified,
    ),
    // The report cannot read the request: request_state "unknown"; only the hint keeps the class.
    case(
        "FOLLOW_SURFACE_UNCERTAIN_UNREAD",
        SURFACE,
        ("uncertain", "", true),
        DU,
        Outcome::NotVerified,
        Outcome::Failed,
    ),
    case(
        "INITIAL_UNCERTAIN_UNREAD",
        INITIAL,
        ("uncertain", "", true),
        DU,
        Outcome::NotVerified,
        Outcome::Failed,
    ),
    // Uncertain with a typed deadline cause: the uncertainty dominates. The wording rules
    // test "timed out" before the pending state, so they said TimedOut where the follow-up
    // context names no uncertainty.
    case(
        "FOLLOW_NATIVE_UNCERTAIN_TIMEOUT",
        NATIVE,
        ("uncertain timeout", "", false),
        DU,
        Outcome::NotVerified,
        Outcome::NotVerified,
    ),
    case(
        "FOLLOW_SURFACE_UNCERTAIN_TIMEOUT",
        SURFACE,
        ("uncertain timeout", "", false),
        DU,
        Outcome::NotVerified,
        Outcome::TimedOut,
    ),
    case(
        "INITIAL_UNCERTAIN_TIMEOUT",
        INITIAL,
        ("uncertain timeout", "", false),
        DU,
        Outcome::NotVerified,
        Outcome::TimedOut,
    ),
    // Not sent.
    case(
        "FOLLOW_TIMEOUT",
        SURFACE,
        ("sent", "timeout", false),
        Some("timed_out"),
        Outcome::TimedOut,
        Outcome::TimedOut,
    ),
    case(
        "FOLLOW_NOT_SENT",
        SURFACE,
        ("not sent", "", false),
        None,
        Outcome::Failed,
        Outcome::Failed,
    ),
    case(
        "INITIAL_NOT_SENT",
        INITIAL,
        ("not sent", "", false),
        None,
        Outcome::Failed,
        Outcome::Failed,
    ),
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
    // Fixture only: the report's read of the recorded request fails while the status
    // record is missing, and the record is restored before any state is checked.
    let status = f.store.directory().join("status.json");
    let hidden = f.store.directory().join("status.json.hidden");
    if case.unread {
        fs::rename(&status, &hidden).unwrap();
    }
    println!("{}_ID {id}", case.label);
    println!("{}_BEGIN", case.label);
    let finished = report.finish(outcome, Map::new());
    println!("{}_END", case.label);
    if case.unread {
        fs::rename(&hidden, &status).unwrap();
    }
    assert!(finished.is_err());
    // The hint changes no lifecycle: one send at most, never a resend, and the Claim
    // retained exactly when the prompt may have been delivered.
    let sends = f.calls().iter().filter(|call| **call == "send").count();
    assert_eq!(sends, usize::from(case.fail.is_empty()), "{}", case.label);
    assert_eq!(
        turn::current_claim_token(&f.store).unwrap().is_some(),
        case.uncertain(),
        "{}",
        case.label
    );
    if case.uncertain() {
        assert!(!f.calls().iter().any(|call| call.starts_with("discard")));
        // The settled cause is recorded as before, without the command's contexts.
        assert_eq!(
            f.store.status().unwrap().error.as_deref(),
            Some(case.cause()),
            "{}",
            case.label
        );
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
    for case in CASES {
        let bytes = section(case.label);
        let session = id(case.label);
        let error = case.error(&session);
        let value: Value = serde_json::from_str(&bytes).unwrap();
        assert_eq!(value["failure_kind"].as_str(), case.hint, "{}", case.label);
        assert_eq!(value["error"], error, "{}", case.label);
        assert_eq!(value["ok"], false);
        assert_eq!(value["schema_version"], 1);
        // The request address survives both a successful read and the fallback.
        assert_eq!(value["session"], session);
        let request = value["request_id"].as_str().unwrap();
        assert!(request.starts_with("request-"), "{}", case.label);
        if case.unread {
            assert_eq!(value["request_state"], "unknown", "{}", case.label);
            assert_eq!(value["result"], Value::Null);
        } else {
            assert_eq!(
                value["request_state"] == "pending",
                case.uncertain(),
                "{}",
                case.label
            );
            assert_ne!(value["request_state"], "unknown", "{}", case.label);
        }
        assert!(value.get("timed_out").is_none());
        // Exactly what Installed builds from a failed child's stdout.
        let reply = reply_from_output(false, bytes.as_bytes(), b"ignored stderr");
        assert_eq!(
            failure(&reply),
            (case.outcome, error.clone()),
            "{}",
            case.label
        );
        assert_eq!(
            legacy_failure(&reply),
            (case.legacy, error.clone()),
            "{}",
            case.label
        );
        let step = step("tell", Instant::now(), &reply);
        assert_eq!(step.outcome, case.outcome);
        assert_eq!(step.request_address, Some(format!("{session}/{request}")));
    }
}
