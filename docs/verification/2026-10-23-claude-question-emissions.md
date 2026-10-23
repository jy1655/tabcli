# Claude question emissions, 2026-10-23

This extends #189 after the Codex question record and `wait --questions` work in #262
and #303. It observes a question emitted in the original bound Claude turn. It does not
deliver answers, replace Claude's dialog, infer an outstanding question, or settle a Request.

## Provider evidence and implementation

The installed Claude Code 2.1.309 generated its declarations in a task-owned native-mode
session's plugin directory. The SHA-256 of `claude-code/index.d.ts` was
`8c37b2b5422de9af9e1a1e8689acc32f4d6e1e38c60e59db92ed0221fc79c770`.
The [official reference](https://code.claude.com/docs/en/plugins/mods/reference) identifies
the installed declarations as authoritative for that build.

`ToolCallInput` has no turn id (line 13529). `TurnStepInput` carries the official turn id,
step index and optional agent id (14366); the completed `TurnStepResult` repeats the turn
and step and contains the requested `toolUses` (14427). `TurnStepToolUse` contains the
name and input (14570). The generated built-in tool declarations give `AskUserQuestion`
its question text, options, header and multi-select flag. Bridge projects only question
text and option labels into its existing query contract.

The question module is in its own session-local plugin, loaded through a second
`--plugin-dir` flag. Claude 2.1.309 rejects two hooks modules in one plugin; this was
reproduced by `claude plugin validate` and a launch without completion readiness
(`session-sIb2mu`, explicitly closed). Both separated plugins passed the ordinary validator,
which listed `session.start`, `turn.start`, `turn.complete` for completion and `turn.complete`,
`turn.step` for questions. Catalog-submission metadata warnings are unrelated to these
private session-local plugins. A policy refusing the question plugin need not disable
the completion plugin; readiness still belongs exclusively to completion.

The question module observes the completed main-loop engine step, passing every streamed chunk
and the response through unchanged. It reads only the final `next.trace` engine/core entry's
`returned` with matching turn/step and `stopReason: tool_use`. An absent or short trace
supplies no evidence. It captures the conversation before awaiting the
step. The receiver requires the exact immutable original binding, current Claim and
Receipt under the existing lifecycle lock. An unbound continuation cannot supply question
authority, even if its later marked completion can settle the Request. The id
`step:N/tool:M` names the provider step and the original tool position, scoped to that
Request. It is an observation cursor, not a question-answer endpoint.

One adapter-private record has a 64 KiB pretty-encoded limit, including request and
binding identity. Duplicate positions preserve the first item; a conflict marks the
record incomplete. A repeated model step, a non-tool-use response carrying a question,
or a lower hook's rewrite also marks incompleteness. Hooks above Bridge can change the
response after observation. The question helper gets two 5-second process attempts;
failure logs unavailable observation and returns the original response. Malformed
attributable items and size omissions retain the previous
prefix. A damaged existing record is not replaced. None of these conditions changes
completion or gives permission to resend input. Queries use historical Receipts and
never repair records or acknowledge questions.

## Component verification

The compiling regression first failed with no question observation after a bound native
question event. After implementation, nine focused Rust tests passed, covering public
result/inspect/wait, read-only queries, cursor suffixes, default wait behavior, completion
precedence, historical evidence, wrong turn/conversation/subagent, late reports after
retirement or into a successor Claim, replay/conflict, malformed and oversized content,
closed and legacy sessions, malformed envelopes, exact 65,536-byte record admission,
historical identity validation, and damaged records without completion changes. The
launch preparation test checks both plugin manifests' module paths and byte equality
of the installed entry points and imported helper files.

The existing JavaScript-to-native-hook integration test now also drives the shipping
streaming handler. It verifies reference-preserving chunks and response, foreign origins,
subagents, model failure, consumer cancellation, mismatched step identity, captured
conversation across a simulated clear, the real Rust record and public wait, identical
helper replay, oversized wire omission, and ordinary completion with the question retained.
It also covers absent trace, session-id failure, repeated indexes with empty second output,
non-tool-use questions and lower-hook rewrites. The latter two assert their own report
wires, independently of an already-incomplete stored record. Completion scenarios load
only the completion module, exercising operation without the optional question module.
This harness passed on native Windows with Node 22.23.3. These are controlled fixtures,
not authenticated provider behavior.

## Authenticated native Windows observation

The feature binary was copied outside `target/` before launch. Its SHA-256 was
`187d9d045ab621da20907cb44a552e0f19d3e712ff6ee1c34a82bd134d68746b`.
It reported 0.4.25 from the source base; it was not a published release or installed binary.
Claude Code was 2.1.309. The launch used the existing trusted workspace, the ordinary state
root, `--terminal windows-console --yolo --prompt-file`, and provider model/effort defaults.

The controlled prompt required one `AskUserQuestion` with a fixed test question and Alpha/Beta
choices, no files, commands or delegation. No answer was supplied by the test harness.

- Session `session-4qHjCr`, Request `request-1792766456045564800-26164-0`.
- `wait --questions` returned exit 0 with `ended: null`, `timed_out: false`, the exact question
  and both labels, emission `step:0/tool:0`, and the still-pending address in `remaining`.
- The Claim token was unchanged and ordinary result remained pending with no published answer.
- The retained record had `incomplete: false`. Because this build admits items only from
  the validated engine/core trace return, this demonstrates that the installed streaming
  module supplied that trace with a normal tool-use result in this run.
- Cursor wait and ordinary wait each timed out without repeating the emission or settling it.
- Explicit close succeeded, with no task-owned child sessions left open.
- The historical result query after close retained the question evidence without an answer.

The ask, wait, inspect, result, timeout, close and summary JSON plus the exact binary remain
in a private local evidence directory outside the checkout build output. No screenshot or
tool-result receipt was used to claim that the question was displayed, pending, or answered.

The same exact binary ran a second, tool-free completion probe in `session-vsdzyU`, Request
`request-1792766553228690500-8144-0`. Both plugins loaded, the original turn bound, and
`wait --questions` returned the marked final answer `CLAUDE_QUESTION_PLUGIN_COMPLETION_OK`
through the ordinary completion path. Explicit close succeeded. This verifies completion
with both plugins present; it does not exercise answering the previous question.

An earlier single-module build also passed the question run (`session-vBXq6n`, Request
`request-1792765151096586200-16304-0`, binary SHA-256
`4d5efe9abfbe853205ef35165742741b9d0a7dae55a88d9eaca7e053689d6bd6`). It read the nearest
step response and did not establish the final trace-based implementation; the repeated
run above is the evidence for that implementation.

## Limits

Model use of the tool was explicitly prompted. This run does not verify unsolicited questions,
operator answers, idle auto-resolution, an interrupted model step, other installed Claude
versions, macOS runtime, legacy/old-module sessions, or questions in unbound continuations.
Subagent and other-plugin exclusions have component coverage. No latency benchmark or
live policy-refusal, outer-hook rewrite or operator-visible dialog assertion was performed.
CI compilation and fixtures
are not authenticated provider runtime. The normal Claude permission path is unchanged.
