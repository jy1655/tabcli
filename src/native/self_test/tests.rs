use super::*;
use serde_json::json;
use std::collections::VecDeque;

struct Fake {
    replies: VecDeque<Reply>,
    calls: Vec<Vec<String>>,
    owned: Option<String>,
    ownership_error: bool,
}

impl Operations for Fake {
    fn call(&mut self, args: &[String]) -> Reply {
        self.calls.push(args.to_vec());
        self.replies.pop_front().expect("unexpected operation")
    }
    fn owned_session(&self) -> Result<Option<String>> {
        if self.ownership_error {
            bail!("unreadable manifest")
        }
        Ok(self.owned.clone())
    }
    fn metadata(&self, _: &str) -> Result<(String, Option<String>)> {
        Ok((
            "test CLI 1.2.3".to_owned(),
            Some("windows-console".to_owned()),
        ))
    }
}

fn ok(value: Value) -> Reply {
    Reply {
        ok: true,
        value,
        error: None,
    }
}
fn error(reason: &str) -> Reply {
    Reply {
        ok: false,
        value: Value::Null,
        error: Some(reason.to_owned()),
    }
}
fn accepted(request: &str) -> Reply {
    ok(json!({"ok": true, "session": "session-owned", "request_id": request}))
}
fn result(request: &str, event: &str) -> Reply {
    ok(
        json!({"ok":true,"session":"session-owned","request_id":request,"event_id":event,"request_state":"completed","result":"AB_marker"}),
    )
}
fn cleanup() -> [Reply; 2] {
    [
        ok(json!({"ok":true,"session":"session-owned","closed":true})),
        ok(json!({"ok":true,"stored_state":"closed"})),
    ]
}
fn fake(replies: impl IntoIterator<Item = Reply>) -> Fake {
    Fake {
        replies: replies.into_iter().collect(),
        calls: Vec::new(),
        owned: Some("session-owned".to_owned()),
        ownership_error: false,
    }
}
fn request(extra: &[&str]) -> Request {
    let mut args = arguments(&["self-test", "claude"]);
    args.extend(arguments(extra));
    let NativeCommand::SelfTest(request) = parse_args(args).unwrap() else {
        panic!()
    };
    request
}
fn run_fake(fake: &mut Fake) -> Report {
    let report = orchestrate(
        &request(&[]),
        fake,
        PathBuf::from("private-root"),
        "AB_marker".to_owned(),
    );
    assert!(fake.replies.is_empty());
    assert_eq!(report.steps.len(), 5);
    assert!(
        fake.calls
            .iter()
            .filter(|args| args[0] == "close-session")
            .all(|args| args
                == &arguments(&["close-session", "session-owned", "--explicit", "--json"]))
    );
    report
}

#[test]
fn identical_answers_require_distinct_requests_and_events() {
    let mut fake = fake(
        [
            accepted("request-1"),
            result("request-1", "event-1.json"),
            accepted("request-2"),
            result("request-2", "event-2.json"),
        ]
        .into_iter()
        .chain(cleanup()),
    );
    let report = run_fake(&mut fake);
    assert_eq!(report.outcome, Outcome::Passed);
    assert_eq!(
        report.steps[1].request_address.as_deref(),
        Some("session-owned/request-1")
    );
    assert_eq!(
        report.steps[3].event_address.as_deref(),
        Some("session-owned/event-2.json")
    );
    assert_eq!(
        fake.calls
            .iter()
            .map(|args| args[0].as_str())
            .collect::<Vec<_>>(),
        [
            "ask",
            "result",
            "tell",
            "result",
            "close-session",
            "inspect"
        ]
    );
    assert!(
        fake.calls[1]
            .windows(2)
            .any(|args| args == ["--request", "request-1"])
    );
    assert!(
        fake.calls[3]
            .windows(2)
            .any(|args| args == ["--request", "request-2"])
    );
    assert_eq!(fake.calls[0][5], fake.calls[2][3]);
}

#[test]
fn failing_ask_still_closes_its_owned_session_without_retry() {
    let mut fake = fake([error("launch failed")].into_iter().chain(cleanup()));
    let report = run_fake(&mut fake);
    assert_eq!(report.outcome, Outcome::Failed);
    assert_eq!(report.steps[4].outcome, Outcome::Passed);
    assert_eq!(fake.calls.len(), 3);
}

#[test]
fn prelaunch_failure_has_no_close_target() {
    let mut fake = fake([error("provider executable not found")]);
    fake.owned = None;
    let report = run_fake(&mut fake);
    assert_eq!(report.outcome, Outcome::Failed);
    assert!(report.session.is_none());
    assert_eq!(fake.calls.len(), 1);
}

#[test]
fn refused_and_uncertain_tell_are_never_repeated() {
    for (reason, outcome) in [
        ("permission refused before sending", Outcome::Failed),
        (
            "Codex terminal follow-up is unavailable: no terminal input was sent",
            Outcome::Unsupported,
        ),
        (
            "provider could not confirm delivery; timed out",
            Outcome::NotVerified,
        ),
        (
            "input receipt was not confirmed; the console paste may have been accepted and is not repeated",
            Outcome::NotVerified,
        ),
        (
            "provider is too old: found 1.0, require >= 2.0",
            Outcome::Unsupported,
        ),
    ] {
        let mut fake = fake(
            [
                accepted("request-1"),
                result("request-1", "event-1.json"),
                error(reason),
            ]
            .into_iter()
            .chain(cleanup()),
        );
        let report = run_fake(&mut fake);
        assert_eq!(report.steps[2].outcome, outcome);
        assert_eq!(report.steps[3].outcome, Outcome::NotVerified);
        assert_eq!(
            fake.calls.iter().filter(|args| args[0] == "tell").count(),
            1
        );
        assert_eq!(report.steps[4].outcome, Outcome::Passed);
    }
}

#[test]
fn timeout_keeps_request_address_and_attempts_cleanup() {
    let mut reply = result("request-1", "event-1.json");
    reply.ok = false;
    reply.value["timed_out"] = json!(true);
    reply.value["request_state"] = json!("pending");
    reply.error = Some("waiting timed out; the request was not cancelled or resent".to_owned());
    let mut fake = fake([accepted("request-1"), reply].into_iter().chain(cleanup()));
    let report = run_fake(&mut fake);
    assert_eq!(report.outcome, Outcome::TimedOut);
    assert_eq!(
        report.steps[1].request_address.as_deref(),
        Some("session-owned/request-1")
    );
    assert_eq!(fake.calls.len(), 4);
}

#[test]
fn wrong_marker_and_unverified_completion_stop_the_round_trip() {
    for (field, value, outcome) in [
        ("result", json!("AB_marker extra"), Outcome::Failed),
        ("request_state", json!("unresolved"), Outcome::NotVerified),
        ("request_state", json!("failed"), Outcome::Failed),
        ("request_id", json!("request-other"), Outcome::Failed),
        ("session", json!("session-foreign"), Outcome::Failed),
        ("event_id", Value::Null, Outcome::NotVerified),
    ] {
        let mut reply = result("request-1", "event-1.json");
        reply.value[field] = value;
        let mut fake = fake([accepted("request-1"), reply].into_iter().chain(cleanup()));
        let report = run_fake(&mut fake);
        assert_eq!(report.outcome, outcome, "{field}");
        assert_eq!(fake.calls.len(), 4);
    }
}

#[test]
fn duplicate_request_or_event_cannot_pass_with_the_right_marker() {
    let mut fake = fake(
        [
            accepted("request-1"),
            result("request-1", "event-1.json"),
            accepted("request-1"),
        ]
        .into_iter()
        .chain(cleanup()),
    );
    assert_eq!(run_fake(&mut fake).steps[2].outcome, Outcome::Failed);
    let mut fake = self::fake(
        [
            accepted("request-1"),
            result("request-1", "event-1.json"),
            accepted("request-2"),
            result("request-2", "event-1.json"),
        ]
        .into_iter()
        .chain(cleanup()),
    );
    assert_eq!(run_fake(&mut fake).steps[3].outcome, Outcome::Failed);
}

#[test]
fn cleanup_failure_retains_session_and_observed_state() {
    let mut fake = fake([
        accepted("request-1"),
        result("request-1", "event-1.json"),
        accepted("request-2"),
        result("request-2", "event-2.json"),
        error("terminal close failed"),
        ok(json!({"ok":true,"stored_state":"working"})),
    ]);
    let report = run_fake(&mut fake);
    assert_eq!(report.outcome, Outcome::Failed);
    assert_eq!(report.session.as_deref(), Some("session-owned"));
    assert_eq!(report.session_state.as_deref(), Some("working"));
    assert_eq!(report.steps[4].outcome, Outcome::Failed);
}

#[test]
fn successful_close_without_closed_state_is_not_verified() {
    let mut replies = cleanup();
    replies[1].value["stored_state"] = json!("working");
    let mut fake = fake([error("ask failed")].into_iter().chain(replies));
    assert_eq!(run_fake(&mut fake).steps[4].outcome, Outcome::NotVerified);
}

#[test]
fn foreign_ask_address_is_never_used_for_cleanup() {
    let mut reply = accepted("request-1");
    reply.value["session"] = json!("session-foreign");
    let mut fake = fake([reply].into_iter().chain(cleanup()));
    assert_eq!(run_fake(&mut fake).outcome, Outcome::NotVerified);
    assert!(
        fake.calls
            .iter()
            .skip(1)
            .all(|args| args[1] == "session-owned")
    );
}

#[test]
fn unreadable_ownership_never_guesses_a_close_target() {
    let mut fake = fake([accepted("request-1")]);
    fake.ownership_error = true;
    let report = run_fake(&mut fake);
    assert_eq!(report.steps[4].outcome, Outcome::NotVerified);
    assert_eq!(report.session.as_deref(), Some("session-owned"));
    assert!(report.session_state.is_none());
    assert_eq!(fake.calls.len(), 1);
}

#[test]
fn self_test_options_follow_explicit_new_session_policy() {
    let request = request(&[
        "--model",
        "chosen",
        "--effort",
        "high",
        "--yolo",
        "--workspace",
        ".",
        "--terminal",
        "windows-console",
        "--timeout-secs",
        "9",
        "--json",
    ]);
    assert_eq!(request.ask.model.as_deref(), Some("chosen"));
    assert_eq!(request.ask.effort.as_deref(), Some("high"));
    assert!(request.ask.yolo && request.ask.json);
    assert_eq!(request.ask.timeout, Duration::from_secs(9));
    let default = self::request(&[]);
    assert!(!default.ask.yolo);
    assert!(default.ask.model.is_none() && default.ask.effort.is_none());
    assert_eq!(default.ask.timeout, Duration::from_secs(120));
    for options in [
        vec!["--prompt", "foreign"],
        vec!["--detach"],
        vec!["--title", "foreign"],
        vec!["--context-result", "session/request"],
        vec!["--timeout-secs", "0"],
        vec!["--yolo", "--yolo"],
        vec!["--model", ""],
    ] {
        let mut args = arguments(&["self-test", "claude"]);
        args.extend(arguments(&options));
        assert!(parse_args(args).is_err(), "{options:?}");
    }
}

#[test]
fn json_report_exposes_all_step_outcomes_and_addresses() {
    let mut fake = fake([error("unsupported terminal")]);
    fake.owned = None;
    let report = run_fake(&mut fake);
    let value = serde_json::to_value(report).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["outcome"], "unsupported");
    assert_eq!(value["steps"][1]["outcome"], "not_verified");
    assert!(value["steps"][0]["request_address"].is_null());
    assert!(value["steps"][0]["event_address"].is_null());
    assert_eq!(value["steps"][4]["outcome"], "passed");
    for outcome in [
        Outcome::Passed,
        Outcome::Failed,
        Outcome::TimedOut,
        Outcome::Unsupported,
        Outcome::NotVerified,
    ] {
        assert!(serde_json::to_value(outcome).unwrap().is_string());
    }
}

#[test]
fn installed_ownership_discovery_is_private_and_read_only() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let installed = Installed {
        executable: PathBuf::from("unused"),
        root: root.path().to_owned(),
    };
    assert!(installed.owned_session().unwrap().is_none());
    let directory = root.path().join("session-owned");
    fs::create_dir(&directory).unwrap();
    let manifest = json!({"schema":1,"id":"session-owned","provider":"claude","provider_path":"unused","provider_version":"2.1.281","workspace":outside.path(),"title":"self-test fixture","model":null,"effort":null,"yolo":false,"created_unix_ms":1});
    write_json_atomic(&directory.join("manifest.json"), &manifest).unwrap();
    let before = fs::read(directory.join("manifest.json")).unwrap();
    fs::create_dir(outside.path().join("session-foreign")).unwrap();
    assert_eq!(
        installed.owned_session().unwrap().as_deref(),
        Some("session-owned")
    );
    assert_eq!(
        installed.metadata("session-owned").unwrap(),
        ("2.1.281".to_owned(), None)
    );
    assert_eq!(fs::read(directory.join("manifest.json")).unwrap(), before);
    assert!(outside.path().join("session-foreign").is_dir());
    let second = root.path().join("session-second");
    fs::create_dir(&second).unwrap();
    let mut manifest = manifest;
    manifest["id"] = json!("session-second");
    write_json_atomic(&second.join("manifest.json"), &manifest).unwrap();
    assert!(installed.owned_session().is_err());
}

#[test]
fn installed_ownership_refuses_mismatched_manifest() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-owned");
    fs::create_dir(&directory).unwrap();
    write_json_atomic(&directory.join("manifest.json"), &json!({"schema":1,"id":"session-foreign","provider":"claude","provider_path":"unused","provider_version":"2.1.281","workspace":root.path(),"title":"fixture","yolo":false,"created_unix_ms":1})).unwrap();
    let installed = Installed {
        executable: PathBuf::from("unused"),
        root: root.path().to_owned(),
    };
    assert!(installed.owned_session().is_err());
}
