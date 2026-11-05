use super::*;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn bridge() -> &'static Path {
    Path::new(if cfg!(windows) {
        r"C:\tabcli.exe"
    } else {
        "/opt/tabcli"
    })
}
fn reply(status: &str, mismatch: bool) -> Value {
    let command = hooks::hook_command(bridge()).unwrap();
    json!({"data":[{"hooks":(["user_prompt_submit", "stop"].map(|label| json!({
        "key":hooks::session_flags_state_key(label),"trustStatus":status,
        "currentHash":if mismatch { "sha256:changed".into() } else { hooks::trust_hash(label, &command) }
    })))}]})
}
// The notice hook's row as Codex 0.166.0 reports it: the matcher is part of its hash.
fn question_row(status: &str, hash: String) -> Value {
    json!({"key":hooks::session_flags_state_key("pre_tool_use"),"trustStatus":status,
        "matcher":hooks::QUESTION_TOOL,"currentHash":hash})
}
fn respond(result: Value) -> (Result<Value>, Capture) {
    let capture = Capture(Arc::default());
    let replies = format!(
        "{}\n{}\n",
        json!({"id":1,"result":{}}),
        json!({"id":2,"result":result})
    );
    (
        exchange(
            capture.clone(),
            Cursor::new(replies.into_bytes()),
            Path::new("workspace"),
            Instant::now() + TIMEOUT,
        ),
        capture,
    )
}

#[test]
fn hook_trust_fake_responder_trusted_untrusted_and_modified_hashes() {
    for (status, mismatch) in [("trusted", false), ("untrusted", false), ("modified", true)] {
        let (result, capture) = respond(reply(status, mismatch));
        let evidence = summarize(&result.unwrap(), bridge(), false).unwrap();
        for hook in evidence["hooks"].as_array().unwrap() {
            assert_eq!(hook["trustStatus"], status);
            assert_eq!(hook["hash_matches"], !mismatch);
            assert_eq!(
                hook["diagnostic"],
                if mismatch {
                    json!("normalization drift")
                } else {
                    Value::Null
                }
            );
        }
        let sent: Vec<Value> = capture
            .0
            .lock()
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect();
        assert_eq!(
            sent,
            vec![
                json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"tabcli","version":env!("CARGO_PKG_VERSION")}}}),
                json!({"method":"initialized"}),
                json!({"id":2,"method":"hooks/list","params":{"cwds":["workspace"]}}),
            ]
        );
    }
}

#[test]
fn hook_trust_checks_the_notice_hook_only_when_a_launch_would_inject_it() {
    use crate::native::settings::CodexLifecycleHooks as Lifecycle;
    use crate::native::settings::CodexQuestionNotice::{self as Notice, Off, On};
    let command = hooks::hook_command(bridge()).unwrap();
    let matched = hooks::matched_trust_hash("pre_tool_use", Some(hooks::QUESTION_TOOL), &command);
    let mut trusted = reply("trusted", false);
    trusted["data"][0]["hooks"]
        .as_array_mut()
        .unwrap()
        .push(question_row("trusted", matched.clone()));
    let evidence = summarize(&trusted, bridge(), true).unwrap();
    let rows = evidence["hooks"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    // Rows follow the injected value's order, whatever order Codex lists the keys in.
    assert_eq!(
        rows.iter()
            .map(|row| row["key"].clone())
            .collect::<Vec<_>>(),
        ["stop", "user_prompt_submit", "pre_tool_use"]
            .map(|label| json!(hooks::session_flags_state_key(label)))
    );
    assert_eq!(
        rows[2]["key"],
        hooks::session_flags_state_key("pre_tool_use")
    );
    assert_eq!(rows[2]["expected_hash"], matched);
    assert_eq!(rows[2]["hash_matches"], true);
    // The matcher-less hash of the same group is drift, as Codex reports it `modified`.
    let mut drifted = reply("trusted", false);
    drifted["data"][0]["hooks"]
        .as_array_mut()
        .unwrap()
        .push(question_row(
            "modified",
            hooks::trust_hash("pre_tool_use", &command),
        ));
    let rows = summarize(&drifted, bridge(), true).unwrap()["hooks"].clone();
    assert_eq!(rows[2]["hash_matches"], false);
    assert_eq!(rows[2]["diagnostic"], "normalization drift");
    // Without the notice hook the third row is neither required nor reported.
    assert_eq!(
        summarize(&trusted, bridge(), false).unwrap()["hooks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(summarize(&reply("trusted", false), bridge(), true).is_err());
    // The probe proposes the notice hook exactly when the launch rule injects it; the gates
    // themselves are the launch tests' matrix.
    let rule = |version, lifecycle, notice: Notice, launcher| {
        matches!(
            hooks::Eligibility::new(version, lifecycle, launcher).notice(|| Ok(notice)),
            Ok(None)
        )
    };
    for (version, lifecycle, notice, launcher, expected) in [
        (Some("codex-cli 0.166.0"), Lifecycle::On, On, true, true),
        (Some("codex-cli 0.166.0"), Lifecycle::On, On, false, false),
        (Some("codex-cli 0.166.0"), Lifecycle::On, Off, true, false),
    ] {
        let case = format!("{version:?} {lifecycle:?} {notice:?} {launcher}");
        assert_eq!(
            rule(version, lifecycle, notice, launcher),
            expected,
            "{case}"
        );
        assert_eq!(
            proposes_notice(version, Ok((lifecycle, notice)), launcher),
            expected,
            "{case}"
        );
    }
    // Unreadable settings enter as the settings' own defaults, which propose no notice hook.
    let unreadable = || Err(anyhow::anyhow!("unreadable settings"));
    for version in [Some("codex-cli 0.166.0"), Some("codex-cli 0.167.2")] {
        assert!(!proposes_notice(version, unreadable(), true), "{version:?}");
        assert_eq!(
            proposes_notice(version, unreadable(), true),
            rule(version, Lifecycle::default(), Notice::default(), true),
            "{version:?}"
        );
    }
}

// For the same Codex version, settings and executable, a new launch records the notice hook as
// injected exactly when the probe proposes it.
#[test]
fn hook_trust_proposes_the_notice_hook_a_launch_records() {
    use crate::native::provider::codex::test_support;
    use crate::native::session::Store;
    let unavailable = Path::new(if cfg!(windows) {
        r"C:\space dir\tabcli.exe"
    } else {
        ""
    });
    for (version, lifecycle, notice, executable, expected) in [
        (Some("codex-cli 0.166.0"), "on", "on", bridge(), true),
        (Some("codex-cli 0.170.3"), "on", "on", bridge(), true),
        (Some("codex-cli 0.166.0"), "off", "on", bridge(), false),
        (Some("codex-cli 0.166.0"), "on", "off", bridge(), false),
        (Some("codex-cli 0.165.9"), "on", "on", bridge(), false),
        (Some("codex-cli 0.163.0"), "on", "on", bridge(), false),
        (Some("0.166.0-alpha.1"), "on", "on", bridge(), false),
        (Some("0.167.0-alpha.1"), "on", "on", bridge(), false),
        (Some("unparseable"), "on", "on", bridge(), false),
        (None, "on", "on", bridge(), false),
        (Some("codex-cli 0.166.0"), "on", "on", unavailable, false),
    ] {
        let case = format!("{version:?} {lifecycle} {notice} {}", executable.display());
        let root = tempfile::tempdir().unwrap();
        let directory = test_support::launch_session(root.path(), version);
        Store::open_unchecked(root.path())
            .private("settings.json")
            .write_json(&json!({"schema":1,"codex_lifecycle_hooks":lifecycle,
                "codex_question_notice":notice}))
            .unwrap();
        hooks::prepare(&directory, executable).unwrap();
        let recorded = hooks::read_notice_mode(&directory)
            .unwrap()
            .unwrap()
            .reason
            .is_none();
        let proposed = proposes_notice(
            version,
            settings(root.path()),
            hooks::hook_command(executable).is_some(),
        );
        assert_eq!(recorded, expected, "{case}");
        assert_eq!(proposed, recorded, "{case}");
    }
}

#[test]
fn hook_trust_fake_responder_rejects_malformed_missing_keys_and_rpc_errors() {
    for bytes in [
        b"not JSON\n".as_slice(),
        b"[]\n",
        b"{\"id\":1,\"error\":{}}\n",
        b"{\"id\":1}\n",
        b"",
    ] {
        assert!(
            exchange(
                Vec::new(),
                Cursor::new(bytes.to_vec()),
                Path::new("."),
                Instant::now() + TIMEOUT
            )
            .is_err()
        );
    }
    for result in [
        json!({}),
        json!({"data":[{"hooks":[]}]}),
        json!({"data":[{}]}),
    ] {
        let (result, _) = respond(result);
        assert!(summarize(&result.unwrap(), bridge(), false).is_err());
    }
    // Keys are checked in the injected value's order, so a reply without any of Bridge's keys
    // names the first of that order.
    let (empty, _) = respond(json!({"data":[{"hooks":[]}]}));
    assert_eq!(
        summarize(&empty.unwrap(), bridge(), true)
            .unwrap_err()
            .to_string(),
        format!(
            "Bridge hook key missing or ambiguous: {}",
            hooks::session_flags_state_key("stop")
        )
    );
}

#[test]
fn hook_trust_fake_responder_timeout_is_bounded() {
    struct Blocked(mpsc::Receiver<()>);
    impl Read for Blocked {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }
    let (release, blocked) = mpsc::channel();
    let error = exchange(Vec::new(), Blocked(blocked), Path::new("."), Instant::now()).unwrap_err();
    assert!(error.to_string().contains("timed out"));
    drop(release);
}

#[test]
fn hook_trust_missing_executable_reports_unavailable_without_files() {
    let root = tempfile::tempdir().unwrap();
    let row = check(&Context {
        directory: Some(root.path()),
        manifest: None,
        executable: None,
        current_version: None,
        workspace: root.path(),
        probe: true,
        deadline: Instant::now(),
        environment: &crate::native::consent::Environment::default(),
    });
    assert_eq!(row.availability, Availability::Unavailable);
    assert_eq!(row.reason_code, "codex_hook_trust_unavailable");
    let value = serde_json::to_value(row).unwrap();
    assert_eq!(value["evidence"]["source"], "current_probe");
    assert!(
        value["detail"]
            .as_str()
            .unwrap()
            .contains("Codex executable unavailable")
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
