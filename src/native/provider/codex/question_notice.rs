//! The injected `PreToolUse` notice hook for Codex's `request_user_input_async` (#342).
//! The tool is asynchronous by design: it emits the question and returns at once while the turn
//! goes on, so a refusal would cut short work the child does in parallel. The hook lets the tool
//! run and adds context for the model instead: no person is at a managed session's terminal, so
//! no reply can arrive during this turn; finish what does not depend on the answer, then end the
//! turn with the question. The transcript observer still records the emission. The notice is
//! advice to the model: a child that ignores it can still wait in its turn.
//! Codex runs the hook only for that tool (the group's matcher) and only in a launch whose notice
//! record says it was injected. It reads that record and nothing else, writes nothing and takes no
//! lock. A command hook must exit 0: exit 2 blocks the call, and any other failure is reported by
//! Codex as a failed hook while the tool runs without the notice.
//! Replace this when Codex offers a verified addressed way to answer a question in the running
//! turn of the injected TUI launch.
use super::correlation::codex_string;
use super::hooks;
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;

/// Codex hands this to the model as a developer message of kind `hooks.additional_context` at the
/// function call (codex-cli 0.166.0, 2026-11-05). The last-line format is the one Claude's
/// question deflection asks for, so a caller reads one format from both providers.
pub(super) const NOTICE: &str = "No person is at this session's terminal, so no reply to this question can arrive during this turn. Do not wait or sleep for one. Finish the work that does not depend on the answer, then end your turn, and end your final message with the one line `QUESTION FOR CALLER: <question> Options: <option labels>`. The caller answers in a follow-up message.";

#[derive(Serialize)]
struct Output {
    #[serde(rename = "hookSpecificOutput")]
    hook_specific_output: HookSpecificOutput,
}

// Context only: no permission decision, reason or input rewrite, so Codex runs the tool as it
// would without the hook (hooks/src/engine/output_parser.rs `parse_pre_tool_use`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HookSpecificOutput {
    hook_event_name: &'static str,
    additional_context: &'static str,
}

/// The one JSON object the hook prints to stdout, if any.
pub(super) fn pre_tool_use(directory: &Path, payload: &Value) -> Result<Option<String>> {
    if codex_string(payload, "tool_name") != Some(hooks::QUESTION_TOOL) {
        return Ok(None);
    }
    // The launch decision, never the current setting. A session without the record was launched
    // by an earlier binary, which injected no notice hook.
    if !hooks::notice_hook_injected(directory)? {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(&Output {
        hook_specific_output: HookSpecificOutput {
            hook_event_name: "PreToolUse",
            additional_context: NOTICE,
        },
    })?))
}

#[cfg(test)]
mod tests;
