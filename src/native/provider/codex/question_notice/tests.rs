use super::*;
use crate::native::provider::NativeProviderAdapter;
use crate::native::provider::codex::{ADAPTER, test_support};
use crate::native::session::Store;
use serde_json::json;

const THREAD: &str = "01234567-89ab-cdef-0123-456789abcdef";

// The exact line the hook prints, with the notice the owner checked live on 2026-11-05.
const EXPECTED: &str = "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"additionalContext\":\"No person is at this session's terminal, so no reply to this question can arrive during this turn. Do not wait or sleep for one. Finish the work that does not depend on the answer, then end your turn, and end your final message with the one line `QUESTION FOR CALLER: <question> Options: <option labels>`. The caller answers in a follow-up message.\"}}";

// The PreToolUse stdin of the notice probes (codex-cli 0.166.0, 2026-11-05).
fn payload(tool: &str) -> Value {
    json!({"session_id":THREAD,"turn_id":"turn-1","transcript_path":"rollout.jsonl",
        "cwd":"workspace","hook_event_name":"PreToolUse","model":"model",
        "permission_mode":"bypassPermissions","tool_name":tool,
        "tool_input":{"questions":[{"title":"Which codename?","options":["ALPHA","BRAVO"]}]},
        "tool_use_id":"call_4a03bcac76544d50b9fbbba3065b36aa"})
}

fn files(directory: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            entries.extend(files(&path));
        } else {
            entries.push((path.clone(), std::fs::read(&path).unwrap()));
        }
    }
    entries.sort();
    entries
}

fn notice_mode(directory: &Path, injected: bool) {
    Store::open_unchecked(directory)
        .private("codex-question-notice.json")
        .write_json(&if injected {
            json!({"schema":1,"notice_hook":"injected","reason":null,
                "vector_codex_version":"0.166.0"})
        } else {
            json!({"schema":1,"notice_hook":"not_injected","reason":"settings_off",
                "vector_codex_version":"0.166.0"})
        })
        .unwrap();
}

#[test]
fn the_notice_is_pinned_and_adds_context_without_any_decision() {
    assert_eq!(
        NOTICE,
        "No person is at this session's terminal, so no reply to this question can arrive during this turn. Do not wait or sleep for one. Finish the work that does not depend on the answer, then end your turn, and end your final message with the one line `QUESTION FOR CALLER: <question> Options: <option labels>`. The caller answers in a follow-up message."
    );
    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_session(root.path(), Some("codex-cli 0.166.0"));
    notice_mode(&directory, true);
    let output = pre_tool_use(&directory, &payload("request_user_input_async"))
        .unwrap()
        .unwrap();
    assert_eq!(output, EXPECTED);
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 1);
    // Context only: a permission decision, its reason, a legacy decision or an input rewrite
    // would change whether or how Codex runs the tool.
    assert_eq!(
        value["hookSpecificOutput"],
        json!({"hookEventName":"PreToolUse","additionalContext":NOTICE})
    );
}

// Claude's deflection reason and this notice ask for one final-line format, so a caller reads
// the same line from both providers.
#[test]
fn the_question_line_format_is_the_one_claude_asks_for() {
    const FORMAT: &str = "QUESTION FOR CALLER: <question> Options: <option labels>";
    assert!(NOTICE.contains(&format!("`{FORMAT}`")));
    let script = include_str!("../../claude/questions.js").replace("\r\n", "\n");
    let start = script.find("const REASON = \"").unwrap() + "const REASON = \"".len();
    let end = start + script[start..].find("\";\n").unwrap();
    let claude: String = script[start..end].split("\" +\n  \"").collect();
    assert!(
        claude.contains("in this form: QUESTION FOR CALLER:"),
        "{claude}"
    );
    assert!(claude.contains(FORMAT), "{claude}");
}

#[test]
fn other_tools_and_sessions_without_the_notice_print_nothing_and_nothing_is_written() {
    for mode in [None, Some(false), Some(true)] {
        let root = tempfile::tempdir().unwrap();
        let directory = test_support::launch_session(root.path(), Some("codex-cli 0.166.0"));
        if let Some(injected) = mode {
            notice_mode(&directory, injected);
        }
        let before = files(&directory);
        for tool in [
            json!("request_user_input_async"),
            json!("request_user_input"),
            json!("Bash"),
            json!("exec_command"),
            json!("mcp__x__request_user_input_async"),
            json!(null),
            json!(7),
        ] {
            let mut payload = payload("request_user_input_async");
            payload["tool_name"] = tool.clone();
            let output = pre_tool_use(&directory, &payload).unwrap();
            if mode == Some(true) && tool == "request_user_input_async" {
                assert_eq!(output.as_deref(), Some(EXPECTED));
            } else {
                assert_eq!(output, None, "{mode:?} {tool}");
            }
            ADAPTER.handle_hook(&directory, &payload).unwrap();
        }
        // The hook takes no claim, changes no status and writes no record.
        assert_eq!(files(&directory), before, "{mode:?}");
    }
}

#[test]
fn only_the_question_tool_reads_the_launch_record_and_an_unreadable_one_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_session(root.path(), Some("codex-cli 0.166.0"));
    Store::open_unchecked(&directory)
        .private("codex-question-notice.json")
        .write_private(b"{")
        .unwrap();
    // Exit 1, never 2: Codex reports the hook as failed and runs the tool without the notice.
    assert!(pre_tool_use(&directory, &payload("request_user_input_async")).is_err());
    assert!(
        ADAPTER
            .handle_hook(&directory, &payload("request_user_input_async"))
            .is_err()
    );
    for tool in ["Bash", "exec_command"] {
        assert_eq!(pre_tool_use(&directory, &payload(tool)).unwrap(), None);
        ADAPTER.handle_hook(&directory, &payload(tool)).unwrap();
    }
}

// The installed command as Codex runs it: `tabcli native-hook codex` with the payload on stdin,
// through the session-directory validation of the hook entry.
#[test]
fn the_hook_binary_prints_one_line_for_the_question_tool_and_exits_zero() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let executable = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("tabcli{}", std::env::consts::EXE_SUFFIX));
    assert!(
        executable.is_file(),
        "missing {}: run cargo test --all-targets to build the sibling binary",
        executable.display()
    );
    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_session(root.path(), Some("codex-cli 0.166.0"));
    let run = |payload: &Value| {
        let mut child = Command::new(&executable)
            .args(["native-hook", "codex"])
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root.path())
            .env("AGENT_BRIDGE_NATIVE_SESSION_DIR", &directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(payload).unwrap())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    for (injected, tool, stdout) in [
        (true, "request_user_input_async", format!("{EXPECTED}\n")),
        (true, "Bash", String::new()),
        (false, "request_user_input_async", String::new()),
    ] {
        notice_mode(&directory, injected);
        let before = files(&directory);
        let output = run(&payload(tool));
        assert_eq!(
            output.status.code(),
            Some(0),
            "{tool}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), stdout, "{tool}");
        assert_eq!(files(&directory), before, "{tool}");
    }
}
