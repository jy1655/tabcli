mod support;
use serde_json::{Value, json};
use std::{fs, process::Command};

#[test]
fn official_module_round_trips_large_and_boundary_wires_through_native_hook_stdin() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("completion.mjs");
    fs::write(
        &module,
        include_str!("../src/native/provider/claude/completion.js"),
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
        let output = Command::new("node")
            .arg(&harness)
            .arg(&module)
            .arg(&directory)
            .arg(env!("CARGO_BIN_EXE_tabcli"))
            .arg(scenario)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root.path())
            .output()
            .expect("Node is required for the official Claude module contract test");
        assert!(
            output.status.success(),
            "{scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
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
            assert_eq!(record["items"].as_array().unwrap().len(), 1);
            assert_eq!(record["incomplete"], scenario.ends_with("oversize"));
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
