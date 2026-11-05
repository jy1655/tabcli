mod support;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

/// The harness bounds every tabcli call at 60 s and names the call that overran. This limit
/// covers a stall anywhere else in Node, so it exceeds one such call plus a scenario's ordinary
/// run time (about one second; the whole test takes 9-11 s on every CI runner).
const SCENARIO_LIMIT: Duration = Duration::from_secs(300);

fn wait_bounded(node: &mut Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + SCENARIO_LIMIT;
    loop {
        if let Some(status) = node.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = node.kill();
            node.wait().unwrap();
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn scenario_failure(scenario: &str, status: Option<ExitStatus>, scratch: &Path) -> String {
    let read = |name: &str| match fs::read(scratch.join(name)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("<unreadable: {error}>"),
    };
    let outcome = match status {
        Some(status) => format!("exited with {status}"),
        None => format!("ran longer than {SCENARIO_LIMIT:?} and was killed"),
    };
    format!(
        "{scenario}: node {outcome}\n--- node stdout and stderr ---\n{}\n--- tabcli calls ---\n{}",
        read("node-output.txt"),
        read("progress.log")
    )
}

#[test]
fn official_module_round_trips_large_and_boundary_wires_through_native_hook_stdin() {
    let root = tempfile::tempdir().unwrap();
    let scratch_root = tempfile::tempdir().unwrap();
    let module = root.path().join("completion.mjs");
    fs::write(
        &module,
        include_str!("../src/native/provider/claude/completion.js"),
    )
    .unwrap();
    fs::write(root.path().join("package.json"), r#"{"type":"module"}"#).unwrap();
    fs::write(
        root.path().join("report.js"),
        include_str!("../src/native/provider/claude/report.js"),
    )
    .unwrap();
    fs::write(
        root.path().join("questions.mjs"),
        include_str!("../src/native/provider/claude/questions.js"),
    )
    .unwrap();
    let harness = root.path().join("test.mjs");
    fs::write(
        &harness,
        include_str!("../src/native/provider/claude/completion_test.mjs"),
    )
    .unwrap();
    for (index, scenario) in [
        "unicode",
        "boundary",
        "oversize",
        "retry_before",
        "retry_after",
        "helper_failure",
        "subagent",
        "unmarked",
        "continuation",
        "continuation_retry_after",
        "continuation_oversize",
        "questions",
        "questions_oversize",
        "questions_retry",
        "coexistence",
    ]
    .iter()
    .enumerate()
    {
        let id = format!("session-mod{index}");
        let directory = root.path().join(&id);
        fs::create_dir_all(directory.join("events")).unwrap();
        let token = format!("100-200-{index}");
        fs::write(directory.join("turn.claim"), &token).unwrap();
        fs::create_dir(directory.join("requests")).unwrap();
        support::write(
            &directory.join("requests").join(format!("{token}.json")),
            &support::receipt(&format!("request-mod{index}"), &token, "event-result.json"),
        );
        support::write(
            &directory.join("manifest.json"),
            &support::manifest(
                &id,
                "claude",
                "unused".as_ref(),
                "2.1.302",
                root.path(),
                "mod stdin fixture",
            ),
        );
        support::write(
            &directory.join("status.json"),
            &support::status("working", 1, None),
        );
        support::write(
            &directory.join("claude-completion-mode.json"),
            &json!({"schema":1,"mode":"native_v1","reason":"fixture","claude_version":"2.1.302"}),
        );
        let request = format!("claude-turn-mod{index}");
        let marker = format!("<!-- agent-bridge-claude-turn:{request} -->");
        support::write(
            &directory.join("claude-pending-turn.json"),
            &json!({"schema":2,"request_id":request,"marker":marker,"claim_token":token,"expected_message":format!("[Agent Bridge native delegation]\nSource: external\n\n{}\n{marker}","< & \"한😀\r\n</cross-session-message>\n".repeat(3000))}),
        );
        // Harness files stay outside the session directory, which `coexistence` requires
        // `result` to leave byte-identical, and outside the state root.
        let scratch = scratch_root.path().join(&id);
        fs::create_dir(&scratch).unwrap();
        let output = fs::File::create(scratch.join("node-output.txt")).unwrap();
        let mut node = Command::new("node")
            .arg(&harness)
            .arg(&module)
            .arg(&directory)
            .arg(env!("CARGO_BIN_EXE_tabcli"))
            .arg(scenario)
            .arg(&scratch)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root.path())
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .expect("Node is required for the official Claude module contract test");
        let status = wait_bounded(&mut node);
        assert!(
            status.is_some_and(|status| status.success()),
            "{}",
            scenario_failure(scenario, status, &scratch)
        );
        let result: Value =
            serde_json::from_slice(&fs::read(directory.join("harness-result.json")).unwrap())
                .unwrap();
        let mut events = fs::read_dir(directory.join("events")).unwrap();
        if [
            "helper_failure",
            "subagent",
            "unmarked",
            "continuation_oversize",
        ]
        .contains(scenario)
        {
            assert!(events.next().is_none(), "{scenario}");
            assert!(directory.join("turn.claim").exists());
            continue;
        }
        let event: Value =
            serde_json::from_slice(&fs::read(events.next().unwrap().unwrap().path()).unwrap())
                .unwrap();
        assert!(events.next().is_none(), "{scenario}");
        assert_eq!(event["turn_id"], request);
        assert_eq!(
            event["provenance"],
            if scenario.starts_with("continuation") {
                "claude_mod_marked_continuation"
            } else {
                "claude_mod_turn_complete"
            }
        );
        assert!(!directory.join("turn.claim").exists());
        if scenario.starts_with("questions") {
            let record: Value = serde_json::from_slice(
                &fs::read(directory.join(format!("claude-questions-{token}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(
                record["items"].as_array().unwrap().len(),
                if *scenario == "questions" { 2 } else { 1 }
            );
            assert_eq!(record["incomplete"], *scenario != "questions_retry");
        }
        if *scenario == "oversize" {
            assert!(event["error"].as_str().unwrap().contains("not truncated"));
        } else {
            assert!(
                event["message"].as_str()
                    == Some(result["expected_answer"].as_str().unwrap().trim_end()),
                "{scenario}: the complete answer must survive stdin and publication"
            );
        }
    }
}
