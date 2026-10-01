use super::*;
use serde_json::json;
use std::collections::VecDeque;

struct Fake {
    replies: VecDeque<Reply>,
    calls: Vec<Vec<String>>,
    sessions: Vec<Value>,
    ownership_error: bool,
    isolated: bool,
}

impl Operations for Fake {
    fn call(&mut self, args: &[String]) -> Reply {
        self.calls.push(args.to_vec());
        self.replies.pop_front().expect("unexpected operation")
    }
    fn owned_session(&self, _: &Path, title: &str) -> Result<Option<String>> {
        if self.ownership_error {
            bail!("unreadable manifest")
        }
        if self.isolated {
            if self.sessions.len() > 1 {
                bail!("self-test state contains more than one session")
            }
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
    assert!(installed.owned_session(workspace.path(), title).is_err());
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
        ok(json!({"ok":true,"session":"session-foreign","request_id":"request-1"})),
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
fn isolated_unreadable_or_ambiguous_ownership_never_closes() {
    for unreadable in [false, true] {
        let mut fake = fake([accepted("request-1")]);
        fake.isolated = true;
        fake.ownership_error = unreadable;
        let report = run_fake(&mut fake);
        assert_eq!(report.steps[4].outcome, Outcome::NotVerified);
        assert_eq!(fake.calls.len(), 1);
    }
}

#[test]
fn installed_commands_override_the_root_only_in_isolated_mode() {
    let root = tempfile::tempdir().unwrap();
    for isolated in [false, true] {
        let installed = Installed {
            executable: PathBuf::from("unused"),
            root: root.path().join("not-created"),
            isolated,
        };
        for command in ["ask", "result", "tell", "close-session", "inspect"] {
            let command = installed.command(&arguments(&[command]));
            let env: Vec<_> = command.get_envs().collect();
            if isolated {
                assert_eq!(
                    env,
                    [(
                        std::ffi::OsStr::new(STATE_DIR_ENV),
                        Some(installed.root.as_os_str())
                    )]
                );
            } else {
                assert!(env.is_empty());
            }
            assert!(!installed.root.exists());
        }
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
    assert!(installed.owned_session(workspace.path(), "unused").is_err());
    manifest["id"] = json!("session-foreign");
    write_json_atomic(&second.join("manifest.json"), &manifest).unwrap();
    assert!(installed.owned_session(workspace.path(), "unused").is_err());
}
