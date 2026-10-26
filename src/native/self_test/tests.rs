use super::*;
use crate::native::session::launch;
use crate::native::session::primitives::STATE_DIR_ENV;
use crate::native::session::primitives::{read_json, write_json_atomic, write_private};
use crate::native::{NativeCommand, parse_args};
use agent_bridge::FirstPartyCli;
use anyhow::{Result, bail};
use serde_json::json;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, thread};

struct Fake {
    replies: VecDeque<Reply>,
    calls: Vec<Vec<String>>,
    budgets: Vec<Duration>,
    sessions: Vec<Value>,
    ownership_error: bool,
    isolated: bool,
}

impl Operations for Fake {
    fn call(&mut self, args: &[String], budget: Duration) -> Reply {
        self.calls.push(args.to_vec());
        self.budgets.push(budget);
        self.replies.pop_front().expect("unexpected operation")
    }
    fn private_sessions(&self) -> Result<Vec<String>> {
        if self.ownership_error {
            bail!("unreadable manifest")
        }
        Ok(self
            .sessions
            .iter()
            .map(|session| session["id"].as_str().unwrap().to_owned())
            .collect())
    }
    fn owned_session(&self, _: &Path, title: &str) -> Result<Option<String>> {
        if self.ownership_error {
            bail!("unreadable manifest")
        }
        if self.isolated {
            Ok(self
                .sessions
                .first()
                .and_then(|session| session["id"].as_str().map(str::to_owned)))
        } else {
            session_with_title(&self.sessions, title)
        }
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
        local_kind: None,
    }
}
fn error(reason: &str) -> Reply {
    Reply {
        ok: false,
        value: Value::Null,
        error: Some(reason.to_owned()),
        local_kind: None,
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
        budgets: Vec::new(),
        sessions: vec![
            json!({"id":"session-foreign","title":"different title"}),
            json!({"id":"session-similar","title":"Agent Bridge self-test AB_marker extra"}),
            json!({"id":"session-owned","title":"Agent Bridge self-test AB_marker"}),
        ],
        ownership_error: false,
        isolated: false,
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
        &request(if fake.isolated { &["--isolated"] } else { &[] }),
        fake,
        PathBuf::from("ordinary-root"),
        "AB_marker".to_owned(),
    );
    assert!(fake.replies.is_empty());
    assert_eq!(report.steps.len(), 5);
    let timeout = CLEANUP_BUDGET.as_secs().to_string();
    assert!(
        fake.calls
            .iter()
            .filter(|args| args[0] == "close-session")
            .all(|args| args
                == &arguments(&[
                    "close-session",
                    "session-owned",
                    "--explicit",
                    "--timeout-secs",
                    &timeout,
                    "--json"
                ]))
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
    assert!(
        fake.calls[0]
            .windows(2)
            .any(|args| args == ["--title", "Agent Bridge self-test AB_marker"])
    );
}

#[test]
fn failing_ask_still_closes_its_owned_session_without_retry() {
    let mut reply = error("launch failed");
    reply.value = json!({"session":"session-owned", "request_id":"request-1"});
    let mut fake = fake([reply].into_iter().chain(cleanup()));
    fake.ownership_error = true;
    let report = run_fake(&mut fake);
    assert_eq!(report.outcome, Outcome::Failed);
    assert_eq!(report.steps[4].outcome, Outcome::Passed);
    assert_eq!(fake.calls.len(), 3);
}

#[test]
fn prelaunch_failure_has_no_close_target() {
    let mut fake = fake([error("provider executable not found")]);
    fake.sessions.clear();
    let report = run_fake(&mut fake);
    assert_eq!(report.outcome, Outcome::Failed);
    assert!(report.session.is_none());
    assert_eq!(fake.calls.len(), 1);
    assert_eq!(report.steps[4].outcome, Outcome::NotVerified);
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
    assert_eq!(report.outcome, Outcome::NotVerified);
    assert_eq!(report.session.as_deref(), Some("session-owned"));
    assert_eq!(report.session_state.as_deref(), Some("working"));
    assert_eq!(report.steps[4].outcome, Outcome::NotVerified);
}

#[test]
fn successful_close_without_closed_state_is_not_verified() {
    let mut replies = cleanup();
    replies[1].value["stored_state"] = json!("working");
    let mut fake = fake([error("ask failed")].into_iter().chain(replies));
    assert_eq!(run_fake(&mut fake).steps[4].outcome, Outcome::NotVerified);
}

#[test]
fn ask_id_is_authoritative_without_title_lookup() {
    let mut fake = fake([error("ask failed")].into_iter().chain(cleanup()));
    fake.replies[0].value = json!({"session":"session-owned"});
    fake.ownership_error = true;
    assert_eq!(run_fake(&mut fake).steps[4].outcome, Outcome::Passed);
}

#[test]
fn ask_without_id_uses_title_lookup_for_cleanup_only() {
    let mut fake = fake(
        [ok(json!({"ok":true,"request_id":"request-1"}))]
            .into_iter()
            .chain(cleanup()),
    );
    let report = run_fake(&mut fake);
    assert_eq!(report.steps[0].outcome, Outcome::NotVerified);
    assert_eq!(report.steps[4].outcome, Outcome::Passed);
    assert_eq!(fake.calls.len(), 3);
}

#[test]
fn unreadable_ownership_never_guesses_a_close_target() {
    let mut fake = fake([error("ask failed")]);
    fake.ownership_error = true;
    let report = run_fake(&mut fake);
    assert_eq!(report.steps[4].outcome, Outcome::NotVerified);
    assert!(report.session.is_none());
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
    assert!(!default.isolated);
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
        vec!["--isolated", "--isolated"],
        vec!["--isolated", "--unknown"],
    ] {
        let mut args = arguments(&["self-test", "claude"]);
        args.extend(arguments(&options));
        assert!(parse_args(args).is_err(), "{options:?}");
    }
}

#[test]
fn json_report_exposes_all_step_outcomes_and_addresses() {
    let mut fake = fake([error("unsupported terminal")]);
    fake.sessions.clear();
    let report = run_fake(&mut fake);
    let value = serde_json::to_value(report).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["outcome"], "unsupported");
    assert_eq!(value["steps"][1]["outcome"], "not_verified");
    assert!(value["steps"][0]["request_address"].is_null());
    assert!(value["steps"][0]["event_address"].is_null());
    assert_eq!(value["steps"][4]["outcome"], "not_verified");
    assert_eq!(value["state_root"], "ordinary-root");
    assert_eq!(value["isolated"], false);
    assert!(value.get("state_directory").is_none());
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
fn exact_title_lookup_never_selects_other_sessions_and_is_read_only() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let installed = Installed {
        executable: PathBuf::from("unused"),
        root: root.path().to_owned(),
        isolated: false,
    };
    let title = "Agent Bridge self-test AB_marker";
    assert!(installed.owned_session(workspace.path(), title).is_err());
    let mut snapshots = Vec::new();
    for (id, session_title) in [
        ("session-foreign", "different title"),
        ("session-similar", "Agent Bridge self-test AB_marker extra"),
        ("session-owned", title),
    ] {
        let directory = root.path().join(id);
        fs::create_dir(&directory).unwrap();
        write_json_atomic(&directory.join("manifest.json"), &json!({"schema":1,"id":id,"provider":"claude","provider_path":"unused","provider_version":"2.1.281","workspace":workspace.path().canonicalize().unwrap(),"title":session_title,"model":null,"effort":null,"yolo":false,"created_unix_ms":1})).unwrap();
        write_json_atomic(&directory.join("status.json"), &json!({"schema":1,"state":"working","pid":4294967295u32,"updated_unix_ms":1,"error":null})).unwrap();
        snapshots.push((
            directory.clone(),
            fs::read(directory.join("status.json")).unwrap(),
            fs::read(directory.join("manifest.json")).unwrap(),
        ));
        if id != "session-owned" {
            assert!(installed.owned_session(workspace.path(), title).is_err());
        }
    }
    assert_eq!(
        installed
            .owned_session(workspace.path(), title)
            .unwrap()
            .as_deref(),
        Some("session-owned")
    );
    assert_eq!(
        installed.metadata("session-owned").unwrap(),
        ("2.1.281".to_owned(), None)
    );
    for (directory, status, manifest) in snapshots {
        assert_eq!(fs::read(directory.join("status.json")).unwrap(), status);
        assert_eq!(fs::read(directory.join("manifest.json")).unwrap(), manifest);
        assert_eq!(fs::read_dir(directory).unwrap().count(), 2);
    }
    let second = root.path().join("session-second");
    fs::create_dir(&second).unwrap();
    let mut manifest: Value = read_json(&root.path().join("session-owned/manifest.json")).unwrap();
    manifest["id"] = json!("session-second");
    write_json_atomic(&second.join("manifest.json"), &manifest).unwrap();
    // Count failure and missing status must not remove an ambiguous ownership row.
    write_private(&second.join("turn.completion.json"), b"broken").unwrap();
    assert!(installed.owned_session(workspace.path(), title).is_err());
    assert_eq!(
        fs::read(second.join("turn.completion.json")).unwrap(),
        b"broken"
    );
    assert_eq!(fs::read_dir(&second).unwrap().count(), 2);
}

#[test]
fn ambiguous_title_lookup_never_closes_anything() {
    for sessions in [
        vec![],
        vec![
            json!({"id":"session-owned","title":"Agent Bridge self-test AB_marker"}),
            json!({"id":"session-second","title":"Agent Bridge self-test AB_marker"}),
        ],
    ] {
        let count = sessions.len();
        let mut fake = fake([error("ask failed without an id")]);
        fake.sessions = sessions;
        let report = run_fake(&mut fake);
        assert_eq!(report.steps[4].outcome, Outcome::NotVerified);
        assert!(
            report.steps[4]
                .reason
                .as_deref()
                .unwrap()
                .contains(&format!("found {count} sessions"))
        );
        assert_eq!(fake.calls.len(), 1);
    }
}

#[test]
fn isolated_option_is_accepted_in_every_option_position() {
    let base = arguments(&["self-test", "claude", "--workspace", ".", "--json"]);
    for position in [1, 2, 4, 5] {
        let mut args = base.clone();
        args.insert(position, "--isolated".to_owned());
        let NativeCommand::SelfTest(request) = parse_args(args).unwrap() else {
            panic!()
        };
        assert!(request.isolated && request.ask.json);
    }
    assert!(
        parse_args(arguments(&[
            "self-test",
            "--isolated",
            "claude",
            "--isolated"
        ]))
        .is_err()
    );
}

#[test]
fn isolated_round_trip_reports_mode_and_uses_the_same_commands() {
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
    fake.isolated = true;
    fake.sessions = vec![json!({"id":"session-owned","title":"irrelevant in private root"})];
    let report = run_fake(&mut fake);
    assert_eq!(report.outcome, Outcome::Passed);
    assert!(report.isolated);
    assert_eq!(fake.calls.len(), 6);
}

#[test]
fn isolated_ownership_is_from_the_directory_even_without_an_ask_id() {
    for ask in [
        error("launch failed"),
        ok(json!({"ok":true,"request_id":"request-1"})),
    ] {
        let mut fake = fake([ask].into_iter().chain(cleanup()));
        fake.isolated = true;
        fake.sessions = vec![json!({"id":"session-owned","title":"unrelated title"})];
        let report = run_fake(&mut fake);
        assert_eq!(report.session.as_deref(), Some("session-owned"));
        assert_eq!(report.steps[4].outcome, Outcome::Passed);
        assert_eq!(fake.calls.len(), 3);
    }
    let mut fake = fake([error("prelaunch failure")]);
    fake.isolated = true;
    fake.sessions.clear();
    assert_eq!(run_fake(&mut fake).steps[4].outcome, Outcome::Passed);
}

#[test]
fn isolated_unreadable_ownership_still_closes_the_reported_session() {
    let mut ask = error("launch failed");
    ask.value = json!({"session":"session-owned"});
    let mut fake = fake([ask].into_iter().chain(cleanup()));
    fake.isolated = true;
    fake.ownership_error = true;
    let report = run_fake(&mut fake);
    assert_eq!(report.steps[4].outcome, Outcome::NotVerified);
    assert_eq!(report.cleanup_sessions[0].outcome, Outcome::Passed);
    assert_eq!(fake.calls.len(), 3);
}

#[test]
fn installed_commands_explicitly_pass_the_ordinary_custom_root() {
    assert_installed_root(false);
}

#[test]
fn installed_commands_explicitly_pass_the_isolated_root() {
    assert_installed_root(true);
}

fn assert_installed_root(isolated: bool) {
    let root = tempfile::tempdir().unwrap();
    let installed = Installed {
        executable: PathBuf::from("unused"),
        root: root.path().join("not-created"),
        isolated,
    };
    for command in ["ask", "result", "tell", "close-session", "inspect"] {
        let command = installed.command(&arguments(&[command]));
        crate::native::process_env::tests::assert_helper_environment(
            &command,
            &[(STATE_DIR_ENV, Some(installed.root.as_os_str()))],
        );
        assert!(!installed.root.exists());
    }
}

#[test]
fn installed_private_ownership_is_read_only_and_ignores_title_and_workspace() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let installed = Installed {
        executable: PathBuf::from("unused"),
        root: root.path().to_owned(),
        isolated: true,
    };
    assert!(
        installed
            .owned_session(workspace.path(), "unused")
            .unwrap()
            .is_none()
    );
    let directory = root.path().join("session-owned");
    fs::create_dir(&directory).unwrap();
    let manifest = json!({"schema":1,"id":"session-owned","provider":"claude","provider_path":"unused","provider_version":"2.1.281","workspace":root.path(),"title":"unrelated","yolo":false,"created_unix_ms":1});
    write_json_atomic(&directory.join("manifest.json"), &manifest).unwrap();
    let before = fs::read(directory.join("manifest.json")).unwrap();
    assert_eq!(
        installed
            .owned_session(workspace.path(), "unused")
            .unwrap()
            .as_deref(),
        Some("session-owned")
    );
    assert_eq!(fs::read(directory.join("manifest.json")).unwrap(), before);
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    let second = root.path().join("session-second");
    fs::create_dir(&second).unwrap();
    let mut manifest = manifest;
    manifest["id"] = json!("session-second");
    write_json_atomic(&second.join("manifest.json"), &manifest).unwrap();
    assert_eq!(
        installed.private_sessions().unwrap(),
        ["session-owned", "session-second"]
    );
    manifest["id"] = json!("session-foreign");
    write_json_atomic(&second.join("manifest.json"), &manifest).unwrap();
    assert_eq!(
        installed.private_sessions().unwrap(),
        ["session-owned", "session-second"]
    );
}

#[test]
fn isolated_cleanup_closes_reported_session_then_every_other_session() {
    let mut ask = error("launch failed");
    ask.value = json!({"session":"session-owned"});
    let mut fake = fake([ask].into_iter().chain(cleanup()).chain([
        ok(json!({"ok":true,"session":"session-other","closed":true})),
        ok(json!({"ok":true,"stored_state":"closed"})),
    ]));
    fake.isolated = true;
    fake.sessions = vec![json!({"id":"session-other"}), json!({"id":"session-owned"})];
    let report = orchestrate(
        &request(&["--isolated"]),
        &mut fake,
        PathBuf::from("private-root"),
        "AB_marker".to_owned(),
    );
    assert_eq!(report.steps[4].outcome, Outcome::Passed);
    assert!(fake.replies.is_empty());
    assert_eq!(
        fake.calls
            .iter()
            .filter(|args| args[0] == "close-session")
            .map(|args| args[1].as_str())
            .collect::<Vec<_>>(),
        ["session-owned", "session-other"]
    );
    let value = serde_json::to_value(report).unwrap();
    assert_eq!(value["cleanup_sessions"][0]["session"], "session-owned");
    assert_eq!(value["cleanup_sessions"][1]["session"], "session-other");
    assert_eq!(value["cleanup_sessions"][1]["outcome"], "passed");
}

#[test]
#[ignore = "subprocess fixture; invoked only by the deadline test"]
fn nonreturning_command_fixture() {
    thread::sleep(Duration::from_secs(1));
}

struct Hanging {
    fake: Fake,
    hang_at: usize,
}
impl Operations for Hanging {
    fn call(&mut self, args: &[String], budget: Duration) -> Reply {
        if self.fake.calls.len() == self.hang_at {
            self.fake.calls.push(args.to_vec());
            let mut installed = Installed {
                executable: std::env::current_exe().unwrap(),
                root: PathBuf::from("unused"),
                isolated: false,
            };
            installed.call(
                &arguments(&[
                    "--exact",
                    "native::self_test::tests::nonreturning_command_fixture",
                    "--ignored",
                ]),
                Duration::from_millis(50),
            )
        } else {
            self.fake.call(args, budget)
        }
    }
    fn private_sessions(&self) -> Result<Vec<String>> {
        self.fake.private_sessions()
    }
    fn owned_session(&self, workspace: &Path, title: &str) -> Result<Option<String>> {
        self.fake.owned_session(workspace, title)
    }
    fn metadata(&self, session: &str) -> Result<(String, Option<String>)> {
        self.fake.metadata(session)
    }
}
#[test]
fn command_deadlines_skip_later_requests_and_still_attempt_cleanup() {
    for hang_at in 0..6 {
        let mut replies = vec![
            accepted("request-1"),
            result("request-1", "event-1.json"),
            accepted("request-2"),
            result("request-2", "event-2.json"),
        ];
        replies.extend(cleanup());
        if hang_at < 4 {
            replies.drain(hang_at..4);
        } else {
            replies.remove(hang_at);
        }
        let mut operations = Hanging {
            fake: fake(replies),
            hang_at,
        };
        let report = orchestrate(
            &request(&[]),
            &mut operations,
            PathBuf::from("unused"),
            "AB_marker".to_owned(),
        );
        let step = &report.steps[hang_at.min(4)];
        assert_eq!(
            step.outcome,
            if hang_at < 4 {
                Outcome::TimedOut
            } else {
                Outcome::NotVerified
            },
            "command {hang_at}"
        );
        assert!(
            step.reason
                .as_deref()
                .unwrap()
                .contains("command did not return")
        );
        assert!(
            operations
                .fake
                .calls
                .iter()
                .any(|args| args[0] == "close-session")
        );
        assert!(operations.fake.replies.is_empty());
    }
}

#[test]
fn isolated_cleanup_attempts_remaining_sessions_after_a_failed_close() {
    let mut ask = error("launch failed");
    ask.value = json!({"session":"session-owned"});
    let mut fake = fake([
        ask,
        error("close timed out; command did not return"),
        ok(json!({"stored_state":"working"})),
        ok(json!({"ok":true,"session":"session-other","closed":true})),
        ok(json!({"ok":true,"stored_state":"closed"})),
    ]);
    fake.isolated = true;
    fake.sessions = vec![json!({"id":"session-owned"}), json!({"id":"session-other"})];
    let report = orchestrate(
        &request(&["--isolated"]),
        &mut fake,
        PathBuf::from("private-root"),
        "AB_marker".to_owned(),
    );
    assert_eq!(report.steps[4].outcome, Outcome::NotVerified);
    assert_eq!(report.cleanup_sessions.len(), 2);
    assert_eq!(report.cleanup_sessions[1].outcome, Outcome::Passed);
    assert!(fake.replies.is_empty());
}

#[test]
fn invalid_reported_session_is_never_a_close_target() {
    let mut ask = error("launch failed");
    ask.value = json!({"session":"../foreign"});
    let mut fake = fake([ask]);
    let report = orchestrate(
        &request(&[]),
        &mut fake,
        PathBuf::from("unused"),
        "AB_marker".to_owned(),
    );
    assert_eq!(fake.calls.len(), 1);
    assert_eq!(report.steps[4].outcome, Outcome::NotVerified);
    assert!(report.cleanup_sessions.is_empty());
}

#[test]
fn retained_launch_surface_cleanup_is_not_verified_until_closed() {
    for closed in [false, true] {
        let retained = "terminal launch failed; exact Ghostty handle retained: u2 tab=t2 window=w1";
        let close = if closed {
            ok(json!({"session":"session-owned", "closed":true}))
        } else {
            error("Ghostty automation timed out before it started")
        };
        let inspect = ok(
            json!({"stored_state": if closed { "closed" } else { "failed" },
            "error": if closed { Value::Null } else { json!(retained) }}),
        );
        let mut fake = fake([error(retained), close, inspect]);
        let report = run_fake(&mut fake);
        let cleanup = &report.steps[4];
        assert_eq!(
            cleanup.outcome,
            if closed {
                Outcome::Passed
            } else {
                Outcome::NotVerified
            }
        );
        assert_ne!(report.outcome, Outcome::Passed);
        assert_eq!(report.cleanup_sessions[0].outcome, cleanup.outcome);
        if !closed {
            let reason = cleanup.reason.as_deref().unwrap();
            for identity in ["session-owned", "Ghostty", "u2", "t2", "w1"] {
                assert!(reason.contains(identity), "{reason}");
            }
        }
    }
}

#[test]
fn closed_residual_surface_cleanup_is_not_verified() {
    let residual = format!(
        "Ghostty u2 tab=t2 window=w1; {}",
        launch::RESIDUAL_SURFACE_MARKER
    );
    for (recorded_error, residual_recorded) in [
        (None, false),
        (Some("ordinary launch error"), false),
        (Some(residual.as_str()), true),
        (Some("the diagnostic wording changed"), true),
        (None, true),
    ] {
        let mut replies = cleanup();
        replies[1].value["error"] = json!(recorded_error);
        if residual_recorded {
            replies[1].value["residual_surface"] = json!("unverified");
        }
        let mut fake = fake(
            [
                accepted("request-1"),
                result("request-1", "event-1.json"),
                accepted("request-2"),
                result("request-2", "event-2.json"),
            ]
            .into_iter()
            .chain(replies),
        );
        let report = run_fake(&mut fake);
        let expected = if residual_recorded {
            Outcome::NotVerified
        } else {
            Outcome::Passed
        };
        assert_eq!(
            report.steps[4].outcome, expected,
            "recorded error={recorded_error:?}"
        );
        assert_eq!(report.cleanup_sessions[0].outcome, expected);
        assert_eq!(report.outcome, expected);
        if expected == Outcome::NotVerified {
            let reason = report.steps[4].reason.as_deref().unwrap();
            assert!(reason.contains("session-owned"));
            if let Some(recorded_error) = recorded_error {
                assert!(reason.contains(recorded_error));
            }
        }
    }
}

#[test]
fn agy_result_timeouts_append_only_the_same_requests_doctor_observation() {
    const REASON: &str = "waiting timed out; the request was not cancelled or resent";
    const DETAIL: &str = "This session's agy.log shows the pending turn waiting for user approval of RunCommand in the terminal. Bridge does not answer it. This is the last observed confirmation, not proof that the dialog is still open.";
    for follow_up in [false, true] {
        for evidence in [
            "matching",
            "other-request",
            "other-session",
            "missing",
            "doctor-error",
        ] {
            let id = if follow_up { "request-2" } else { "request-1" };
            let mut replies = vec![accepted("request-1")];
            if follow_up {
                replies.extend([result("request-1", "event-1.json"), accepted("request-2")]);
            }
            let mut timeout = error(REASON);
            timeout.value = json!({"session":"session-owned", "request_id": id, "timed_out":true});
            replies.push(timeout);
            replies.push(match evidence {
                "doctor-error" => error("doctor timed out"),
                "missing" => ok(json!({"session":"session-owned", "checks":[]})),
                _ => ok(json!({"session":if evidence == "other-session" {"session-other"} else {"session-owned"}, "checks":[{
                    "reason_code":"agy_tool_confirmation_observed", "detail":DETAIL,
                    "evidence":{"request_id":if evidence == "other-request" {"request-other"} else {id}}
                }]})),
            });
            replies.extend(cleanup());
            let mut fake = fake(replies);
            let mut req = request(&[]);
            req.ask.provider = FirstPartyCli::Agy;
            let report = orchestrate(
                &req,
                &mut fake,
                PathBuf::from("root"),
                "AB_marker".to_owned(),
            );
            let step = &report.steps[if follow_up { 3 } else { 1 }];
            assert_eq!(step.outcome, Outcome::TimedOut);
            assert_eq!(
                step.reason.as_deref(),
                Some(
                    if evidence == "matching" {
                        format!("{REASON}; {DETAIL}")
                    } else {
                        REASON.to_owned()
                    }
                    .as_str()
                )
            );
            assert_eq!(report.steps[4].outcome, Outcome::Passed);
            assert!(fake.replies.is_empty());
            assert_eq!(
                fake.calls
                    .iter()
                    .filter(|args| args[0] == "doctor")
                    .collect::<Vec<_>>(),
                vec![&arguments(&["doctor", "session-owned", "--json"])]
            );
            assert_eq!(
                fake.calls.iter().filter(|args| args[0] == "tell").count(),
                usize::from(follow_up)
            );
        }
    }
}

#[test]
fn pi_result_timeouts_append_session_credentials_without_resending() {
    const REASON: &str = "waiting timed out; the request was not cancelled or resent";
    for follow_up in [false, true] {
        for evidence in [
            "ready",
            "not_ready",
            "unknown",
            "other-session",
            "missing",
            "doctor-error",
        ] {
            let id = if follow_up { "request-2" } else { "request-1" };
            let mut replies = vec![accepted("request-1")];
            if follow_up {
                replies.extend([result("request-1", "event-1.json"), accepted("request-2")]);
            }
            let mut timeout = error(REASON);
            timeout.value = json!({"session":"session-owned", "request_id":id, "timed_out":true});
            replies.push(timeout);
            let detail = format!(
                "Pi provider credentials {evidence} for openai: diagnostic observation only."
            );
            replies.push(match evidence {
                "doctor-error" => error("doctor timed out"),
                "missing" => ok(json!({"session":"session-owned", "checks":[]})),
                _ => ok(json!({"session":if evidence == "other-session" {"other"} else {"session-owned"}, "checks":[{
                    "id":"pi_provider_credentials", "detail":detail, "evidence":{"status":evidence}
                }]})),
            });
            replies.extend(cleanup());
            let mut fake = fake(replies);
            let mut req = request(&[]);
            req.ask.provider = FirstPartyCli::Pi;
            let report = orchestrate(
                &req,
                &mut fake,
                PathBuf::from("root"),
                "AB_marker".to_owned(),
            );
            let step = &report.steps[if follow_up { 3 } else { 1 }];
            assert_eq!(step.outcome, Outcome::TimedOut);
            let expected = if matches!(evidence, "ready" | "not_ready" | "unknown") {
                format!("{REASON}; {detail}")
            } else {
                REASON.to_owned()
            };
            assert_eq!(step.reason.as_deref(), Some(expected.as_str()));
            assert_eq!(report.steps[4].outcome, Outcome::Passed);
            assert!(fake.replies.is_empty());
            assert_eq!(
                fake.calls
                    .iter()
                    .filter(|args| args[0] == "doctor")
                    .collect::<Vec<_>>(),
                vec![&arguments(&[
                    "doctor",
                    "session-owned",
                    "--probe",
                    "--json"
                ])]
            );
            assert_eq!(
                fake.calls.iter().filter(|args| args[0] == "tell").count(),
                usize::from(follow_up)
            );
        }
    }
}

#[test]
fn all_providers_keep_the_exact_marker_contract_without_tools() {
    for provider in [
        FirstPartyCli::Agy,
        FirstPartyCli::Codex,
        FirstPartyCli::Claude,
        FirstPartyCli::Pi,
    ] {
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
        let mut req = request(&[]);
        req.ask.provider = provider;
        let report = orchestrate(
            &req,
            &mut fake,
            PathBuf::from("root"),
            "AB_marker".to_owned(),
        );
        assert_eq!(report.outcome, Outcome::Passed);
        assert_eq!(
            fake.calls[0][5],
            "No tool, command, or file is needed. Reply with exactly this marker and nothing else: AB_marker"
        );
        assert_eq!(fake.calls[0][5], fake.calls[2][3]);
        assert!(!fake.calls.iter().any(|args| args[0] == "doctor"));
    }
}

#[test]
fn close_deadline_cleanup_budgets() {
    let mut fake = fake([error("ask failed")].into_iter().chain(cleanup()));
    run_fake(&mut fake);
    for (args, budget) in fake.calls.iter().zip(&fake.budgets) {
        if args[0] == "close-session" {
            assert_eq!(*budget, CLEANUP_BUDGET + COMMAND_MARGIN);
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--timeout-secs", &CLEANUP_BUDGET.as_secs().to_string()])
            );
        } else if args[0] == "inspect" {
            assert_eq!(*budget, CLEANUP_BUDGET);
        }
    }
}

#[cfg(unix)]
#[test]
fn close_deadline_child_report_and_outer_timeout_continue_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    for hangs in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("private-close-fixture");
        fs::write(&executable, if hangs {
            "#!/bin/sh\nexec /bin/sleep 60\n"
        } else {
            "#!/bin/sh\nprintf '%s\\n' '{\"ok\":false,\"error\":\"close deadline expired; use --timeout-secs\"}'\nexit 1\n"
        }).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut installed = Installed {
            executable,
            root: directory.path().into(),
            isolated: true,
        };
        let reply = installed.call(
            &arguments(&[
                "close-session",
                "session-owned",
                "--explicit",
                "--timeout-secs",
                "10",
            ]),
            CLEANUP_BUDGET + COMMAND_MARGIN,
        );
        assert!(!reply.ok);
        assert!(reply.error.as_deref().unwrap().contains(if hangs {
            "did not return"
        } else {
            "close deadline"
        }));
        let mut ask = error("launch failed");
        ask.value = json!({"session":"session-owned"});
        let mut fake = fake([
            ask,
            reply,
            ok(json!({"stored_state":"working"})),
            ok(json!({"ok":true,"session":"session-other","closed":true})),
            ok(json!({"ok":true,"stored_state":"closed"})),
        ]);
        fake.isolated = true;
        fake.sessions = vec![json!({"id":"session-owned"}), json!({"id":"session-other"})];
        let report = orchestrate(
            &request(&["--isolated"]),
            &mut fake,
            directory.path().into(),
            "AB_marker".into(),
        );
        assert_eq!(report.cleanup_sessions.len(), 2);
        assert_eq!(report.cleanup_sessions[0].outcome, Outcome::NotVerified);
        assert_eq!(report.cleanup_sessions[1].outcome, Outcome::Passed);
        assert!(fake.replies.is_empty());
    }
}
#[test]
fn delivery_wording_preserves_self_test_failure_classification() {
    for (reason, expected) in [
        (
            "provider follow-up transport provider-cross-session-message could not confirm delivery; the turn remains claimed until the target reports completion or the session is explicitly closed: cause",
            Outcome::NotVerified,
        ),
        (
            "provider follow-up transport provider-native-queue could not confirm delivery; the turn remains claimed until the target reports completion or the session is explicitly closed: cause",
            Outcome::NotVerified,
        ),
        (
            "provider follow-up transport terminal-paste-fallback failed: failed to type into visible Windows Console session session-test: cause",
            Outcome::Failed,
        ),
        (
            "provider follow-up transport terminal-paste-fallback exhausted its total timeout before delivery: timed out after 30 seconds",
            Outcome::TimedOut,
        ),
        (
            "Claude initial cross-session delivery could not be confirmed; the turn remains claimed until completion or explicit close: cause",
            Outcome::Failed,
        ),
        (
            "Claude initial cross-session delivery was not sent: cause",
            Outcome::Failed,
        ),
    ] {
        let (actual, retained) = failure(&error(reason));
        assert_eq!(actual, expected);
        assert_eq!(retained, reason);
    }
}

#[test]
fn an_admission_refusal_reports_one_failed_step_and_no_root_for_isolated_mode() {
    let refusal = anyhow::anyhow!("launch refused: fixture");
    for (isolated, root) in [(true, None), (false, Some(PathBuf::from("ordinary-root")))] {
        let request = request(if isolated { &["--isolated"] } else { &[] });
        let report = refused(
            &request,
            root.clone(),
            "AB_marker".to_owned(),
            Instant::now(),
            &refusal,
        );
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["outcome"], "failed");
        assert_eq!(value["isolated"], isolated);
        assert_eq!(value["state_root"], json!(root));
        assert_eq!(value["marker"], "AB_marker");
        assert!(value["session"].is_null());
        assert!(value["session_state"].is_null());
        assert_eq!(value["cleanup_sessions"], json!([]));
        let steps = value["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["name"], "admission");
        assert_eq!(steps[0]["outcome"], "failed");
        assert!(steps[0]["elapsed_ms"].is_u64());
        assert!(steps[0]["request_address"].is_null());
        assert!(steps[0]["event_address"].is_null());
        assert_eq!(steps[0]["reason"], "launch refused: fixture");
    }
}

#[test]
fn admission_resolves_the_workspace_before_any_root_or_command() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let missing_request = request(&["--workspace", missing.to_str().unwrap(), "--isolated"]);
    let error = admit(&missing_request, None).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("workspace does not exist or cannot be resolved"),
        "{error:#}"
    );
    let file = directory.path().join("file");
    fs::write(&file, b"").unwrap();
    let file_request = request(&["--workspace", file.to_str().unwrap()]);
    let root = directory.path().join("state");
    let error = admit(&file_request, Some(&root)).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("workspace is not a directory"),
        "{error:#}"
    );
    assert!(!root.exists());
}

mod connection;

fn hinted(value: Value, reason: Option<&str>) -> Reply {
    Reply {
        ok: false,
        value,
        error: reason.map(str::to_owned),
        local_kind: None,
    }
}

#[test]
fn a_recognized_hint_decides_the_class_with_precedence_over_timed_out_and_text() {
    for (value, reason, expected) in [
        (
            json!({"failure_kind": "delivery_uncertain", "timed_out": true}),
            Some("waiting timed out"),
            Outcome::NotVerified,
        ),
        (
            json!({"failure_kind": "delivery_uncertain"}),
            Some("unsupported settings schema 2"),
            Outcome::NotVerified,
        ),
        (
            json!({"failure_kind": "timed_out"}),
            Some("could not confirm delivery"),
            Outcome::TimedOut,
        ),
        (
            json!({"failure_kind": "timed_out"}),
            Some("unsupported"),
            Outcome::TimedOut,
        ),
        (
            json!({"failure_kind": "unsupported", "timed_out": true}),
            Some("gate"),
            Outcome::TimedOut,
        ),
        (
            json!({"failure_kind": "unsupported"}),
            Some("may have been accepted; timed out"),
            Outcome::Unsupported,
        ),
        // Hint without an error: the class still comes from the hint, with the default reason.
        (
            json!({"failure_kind": "unsupported", "request_state": "pending"}),
            None,
            Outcome::Unsupported,
        ),
    ] {
        let reply = hinted(value.clone(), reason);
        let (outcome, retained) = failure(&reply);
        assert_eq!(outcome, expected, "{value}");
        assert_eq!(
            retained,
            reason.unwrap_or("command did not provide a verified outcome"),
            "{value}"
        );
    }
}

#[test]
fn absent_unknown_and_malformed_hints_keep_the_wording_classification_exactly() {
    // The legacy matrix, including its accidental unsupported matches.
    let reasons = [
        Some("provider follow-up transport x could not confirm delivery: cause"),
        Some("Agy input receipt was not confirmed; the console paste may have been accepted"),
        Some("timed out after 30 seconds"),
        Some("unsupported settings schema 2 in /tmp/settings.json"),
        Some("unsupported double quote in the Windows console command"),
        Some("unsupported CLI fixture"),
        Some("visible terminal sessions are not supported on this operating system"),
        Some("codex is too old: found \"codex 0.1\", require >= 0.2"),
        Some("Codex follow-up is unavailable: no terminal input was sent"),
        Some("permission refused"),
        None,
    ];
    let hints = [
        None,
        Some(json!(null)),
        Some(json!("not_sent")),
        Some(json!("Timed_Out")),
        Some(json!(7)),
        Some(json!(["timed_out"])),
        Some(json!({"kind": "unsupported"})),
    ];
    for reason in reasons {
        for hint in &hints {
            for extra in [
                json!({}),
                json!({"timed_out": true}),
                json!({"request_state": "pending"}),
            ] {
                let mut value = extra.clone();
                if let Some(hint) = hint {
                    value["failure_kind"] = hint.clone();
                }
                let reply = hinted(value.clone(), reason);
                assert_eq!(
                    failure(&reply),
                    legacy_failure(&reply),
                    "{value} {reason:?}"
                );
            }
        }
    }
    // The pinned legacy cases still classify as before when no hint exists.
    assert_eq!(
        failure(&error("unsupported settings schema 2")).0,
        Outcome::Unsupported
    );
    assert_eq!(failure(&error("permission refused")).0, Outcome::Failed);
}

#[test]
fn a_successful_reply_passes_whatever_hint_it_carries() {
    for kind in ["delivery_uncertain", "timed_out", "unsupported"] {
        let mut reply = ok(json!({"ok": true, "failure_kind": kind}));
        reply.local_kind = Some(crate::native::failure_kind::FailureKind::TimedOut);
        let step = step("ask", Instant::now(), &reply);
        assert_eq!(step.outcome, Outcome::Passed);
        assert!(step.reason.is_none());
    }
}

#[test]
fn an_inspect_hint_during_cleanup_still_leaves_cleanup_not_verified() {
    let mut inspect = error("inspect failed: timed out after 10 seconds");
    inspect.value = json!({"failure_kind": "unsupported", "stored_state": "working"});
    let mut fake = fake([
        accepted("request-1"),
        result("request-1", "event-1.json"),
        accepted("request-2"),
        result("request-2", "event-2.json"),
        ok(json!({"ok":true,"session":"session-owned","closed":true})),
        inspect,
    ]);
    let report = run_fake(&mut fake);
    let cleanup = report.steps.last().unwrap();
    assert_eq!(cleanup.outcome, Outcome::NotVerified);
    assert!(
        cleanup
            .reason
            .as_deref()
            .unwrap()
            .contains("inspect failed: timed out after 10 seconds")
    );
}

#[test]
fn the_self_test_runner_classifies_its_own_deadlines_from_typed_evidence() {
    use crate::native::failure_kind::FailureKind;
    let mut installed = Installed {
        executable: std::env::current_exe().unwrap(),
        root: PathBuf::from("unused"),
        isolated: false,
    };
    // Before the spawn: the budget is already spent.
    let reply = installed.call(
        &arguments(&[
            "--exact",
            "native::self_test::tests::nonreturning_command_fixture",
            "--ignored",
        ]),
        Duration::ZERO,
    );
    assert!(!reply.ok && reply.value.is_null());
    assert_eq!(reply.local_kind, Some(FailureKind::TimedOut));
    assert_eq!(
        reply.error.as_deref(),
        Some("self-test --exact command did not return timed out before it started")
    );
    assert_eq!(failure(&reply).0, Outcome::TimedOut);
    // After the spawn: the child is ended at the deadline.
    let reply = installed.call(
        &arguments(&[
            "--exact",
            "native::self_test::tests::nonreturning_command_fixture",
            "--ignored",
        ]),
        Duration::from_millis(50),
    );
    assert_eq!(reply.local_kind, Some(FailureKind::TimedOut));
    assert_eq!(
        reply.error.as_deref(),
        Some("self-test --exact command did not return timed out")
    );
    assert_eq!(failure(&reply).0, Outcome::TimedOut);
    // A start failure is no deadline: the wording classification applies.
    installed.executable = PathBuf::from("definitely-not-an-agent-bridge-executable");
    let reply = installed.call(&arguments(&["ask"]), Duration::from_secs(5));
    assert_eq!(reply.local_kind, None);
    assert!(
        reply
            .error
            .as_deref()
            .unwrap()
            .starts_with("failed to start self-test ask command did not return")
    );
    assert_eq!(failure(&reply), legacy_failure(&reply));
    assert_eq!(failure(&reply).0, Outcome::Failed);
}
