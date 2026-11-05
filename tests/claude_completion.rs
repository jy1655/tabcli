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
    fs::write(root.path().join("package.json"), r#"{"type":"module"}"#).unwrap();
    fs::write(
        root.path().join("report.js"),
        include_str!("../src/native/provider/claude/report.js"),
    )
    .unwrap();
    // The question module imports the launch switch that prepare writes; a second copy
    // runs with deflection off.
    for (directory, deflect) in [
        (root.path().to_owned(), true),
        (root.path().join("off"), false),
    ] {
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("questions.mjs"),
            include_str!("../src/native/provider/claude/questions.js"),
        )
        .unwrap();
        fs::write(
            directory.join("config.js"),
            format!("export const deflect = {deflect};\n"),
        )
        .unwrap();
    }
    fs::write(
        root.path().join("off").join("report.js"),
        include_str!("../src/native/provider/claude/report.js"),
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
        "questions_deflection",
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
            assert_eq!(
                record["items"].as_array().unwrap().len(),
                match *scenario {
                    "questions" => 2,
                    "questions_deflection" => 8,
                    _ => 1,
                }
            );
            assert_eq!(record["incomplete"], *scenario != "questions_retry");
            // Only the deflection scenario's refused and attributed calls carry the flag.
            let deflected: Vec<_> = record["items"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item.get("deflected").is_some())
                .map(|item| (item["id"].as_str().unwrap(), item["deflected"].clone()))
                .collect();
            if *scenario == "questions_deflection" {
                assert_eq!(
                    deflected,
                    ["step:5/tool:1", "step:6/tool:1", "step:7/tool:1"]
                        .map(|id| (id, Value::Bool(true)))
                );
            } else {
                assert!(deflected.is_empty(), "{scenario}");
            }
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
