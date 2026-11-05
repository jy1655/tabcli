# Answering a Codex child's question on macOS, 2026-11-05

Owner live measurement for #342 on one Mac. It covers the four points the issue asks about:
what happens to the turn when a Codex child asks with its structured question tool, where a
`codex queue` message goes while that turn is running, how an ordinary `tell` continues after
the turn has ended, and whether a prose question behaves differently. A last run probes one
way to handle it, with a temporary build. Each case was run once, with one model. Nothing in
the product changed.

## Environment

- macOS 26.5.2 (25F84), Apple Silicon, iTerm2 3.7.7, codex-cli 0.166.0, model `gpt-6-astra`
  (the configured default) at `--effort medium`. The screen was unlocked: `ioreg` showed no
  `CGSSessionScreenIsLocked` key before the runs.
- Debug build of main `14c1f48` plus a temporary patch in `src/native/hook.rs` that writes
  each `native-hook` payload to a file exactly as received, hook stdin and notify argv alike.
  SHA-256 `9d151df5006b0799d92a7b59345713f3ae95514dde194a6e97a711457d5f921b`. It was run by
  its absolute path with `AGENT_BRIDGE_NATIVE_STATE_DIR` pointing at an empty private
  directory. The patch was never committed.
- Workspace: the repository checkout, already trusted by Codex. Options:
  `--terminal iterm2 --yolo --effort medium`. `codex-launch-mode.json` recorded
  `lifecycle_hooks: "injected"` for every session.
- Every task prompt (the initial prompt of each run and the prose-question task) told the
  child not to read or modify files; the answers did not repeat it. The raw lines below are
  copied from the hook captures and the rollout transcripts. The home directory is written
  `<home>`, long texts are cut where marked, and transcript lines leave out `ordinal`, record
  ids, `metadata` and, where it holds only the turn id and `create_time`, the metadata
  passthrough. Hook payloads are whole.

Codex's own description of the tool, from the tool list in the transcript:

> Ask the user one or more questions during ongoing work. […] The tool returns immediately
> without ending the turn or waiting for a reply; any reply arrives asynchronously as a new
> user message.

## Run A: a queued message while the bound turn is running

The prompt: ask for a codename with `request_user_input_async` (options `ALPHA`, `BRAVO`), run
`sleep 75` once, then report `CODENAME=<answer>` with the first characters of the message that
carried it, or `NO_ANSWER_YET` if nothing had arrived.

| step | command | result |
|---|---|---|
| 1 | `ask codex ... --prompt-file a1.md --detach --json` | `ok: true`, session and request ids after 3 s |
| 2 | `wait <session>/<request> --questions --timeout-secs 60 --json` | after 8 s: `ended: null`, one emission (`What is the codename?`, options `ALPHA`, `BRAVO`), `request_state: pending`, `session_state: running` |
| 3 | `codex queue --thread <thread> --message "The codename is BRAVO."` | exit 0, `Queued message 01a1aa60-9645-76d2-b77c-b87b5264575a for thread 01a1aa60-5d71-7220-a929-7b15e7ff637d.` |
| 4 | `screen <session> --json`, 3 s and 18 s after step 3 | the question with its two options, `Waiting for background terminal` with `sleep 75`, and a panel `Queued follow-up inputs` / `? 1 question` / `⇧← to answer`; the queued text is not on the screen |
| 5 | `result <session> --request <id> --wait --timeout-secs 180 --json` | `request_state: completed`, Result `NO_ANSWER_YET`, `question_emitted: true`, `question_emission_count: 1`, the bound turn's id, 89,477 ms observed |
| 6 | `screen <session> --json`, 4 s later | below `NO_ANSWER_YET`: a new user row `The codename is BRAVO.` and the answer `CODENAME=BRAVO` / `ARRIVED_AS=The codename is BRAVO.` |
| 7 | `result <session> --list --json`, `inspect <session> --json` | one request; `session_state: ready`; `capabilities.questions.response`: `unsupported` / `no_addressed_answer_path` |

Step 3 ran in the workspace with a minimal environment (`HOME`, `PATH`, `USER`, `LOGNAME`,
`SHELL`, `TMPDIR`, `LANG`, `TERM`) and `AGENT_BRIDGE_NATIVE_SESSION_DIR` set to the session
directory, which Bridge's own queue command also sets. The text was not framed as a Bridge
prompt. `tell` could not be used: a pending request refuses it (run B, step 6).

The question, and the tool's return to the model (transcript):

```json
{"timestamp":"2026-11-05T06:23:29.942Z","type":"event_msg","payload":{"type":"item_completed","thread_id":"01a1aa60-5d71-7220-a929-7b15e7ff637d","turn_id":"01a1aa60-6560-7411-912e-cbb732c453cd","item":{"type":"AgentMessage","id":"call_0a143a7750374af9aa8e8ae8bc75f83a","content":[{"type":"Text","text":"What is the codename?\n- ALPHA\n- BRAVO"}],"phase":"final_answer","delivery":"async","questions":[{"title":"What is the codename?","options":["ALPHA","BRAVO"]}]},"started_at_ms":1793859809941,"completed_at_ms":1793859809942}}
{"timestamp":"2026-11-05T06:23:29.999Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_0a143a7750374af9aa8e8ae8bc75f83a","output":"{\"accepted\":true}"}}
```

The queue had accepted the message by 06:23:39 UTC. The turn then made six more model requests
(the last tool output arrived at 06:24:47) and its transcript holds no user record after the
framed prompt. The turn ended, and the queued message started the next one 18 ms later:

```json
{"timestamp":"2026-11-05T06:24:49.280Z","type":"event_msg","payload":{"type":"task_complete","root_turn_id":"01a1aa60-6560-7411-912e-cbb732c453cd","turn_id":"01a1aa60-6560-7411-912e-cbb732c453cd","last_agent_message":"NO_ANSWER_YET","started_at":1793859806,"completed_at":1793859889,"duration_ms":82701,"time_to_first_token_ms":3197}}
{"timestamp":"2026-11-05T06:24:49.298Z","type":"event_msg","payload":{"type":"task_started","turn_attribution":{"turn_id":"01a1aa61-a883-7540-9627-206d9a7e3ecf","turn_trigger":"queue","parent_turn_id":null,"initiating_agent_path":null,"root_turn_id":"01a1aa61-a883-7540-9627-206d9a7e3ecf"},"turn_id":"01a1aa61-a883-7540-9627-206d9a7e3ecf","root_turn_id":"01a1aa61-a883-7540-9627-206d9a7e3ecf","started_at":1793859889,"model_context_window":258400,"collaboration_mode_kind":"default"}}
{"timestamp":"2026-11-05T06:24:49.396Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"The codename is BRAVO."}],"internal_chat_message_metadata_passthrough":{"turn_id":"01a1aa61-a883-7540-9627-206d9a7e3ecf","create_time":1793859889.39615,"content_item_kinds":["user.text"],"content_item_metadata":[{"provenance":{"type":"user"}}]}}}
{"timestamp":"2026-11-05T06:24:52.090Z","type":"event_msg","payload":{"type":"task_complete","root_turn_id":"01a1aa61-a883-7540-9627-206d9a7e3ecf","turn_id":"01a1aa61-a883-7540-9627-206d9a7e3ecf","last_agent_message":"CODENAME=BRAVO\nARRIVED_AS=The codename is BRAVO.","started_at":1793859889,"completed_at":1793859892,"duration_ms":2792,"time_to_first_token_ms":2268}}
```

`UserPromptSubmit` fired for the queued message with the new turn's id, and `Stop` and the
legacy notify followed for that turn (hook stdin, then notify argv):

```json
{"session_id":"01a1aa60-5d71-7220-a929-7b15e7ff637d","turn_id":"01a1aa61-a883-7540-9627-206d9a7e3ecf","transcript_path":"<home>/.codex/sessions/2026/11/05/rollout-2026-11-05T15-23-24-01a1aa60-5d71-7220-a929-7b15e7ff637d.jsonl","cwd":"<home>/Developments/ForTest/agent-bridge","hook_event_name":"UserPromptSubmit","model":"gpt-6-astra","permission_mode":"bypassPermissions","prompt":"The codename is BRAVO."}
{"session_id":"01a1aa60-5d71-7220-a929-7b15e7ff637d","turn_id":"01a1aa61-a883-7540-9627-206d9a7e3ecf","transcript_path":"<home>/.codex/sessions/2026/11/05/rollout-2026-11-05T15-23-24-01a1aa60-5d71-7220-a929-7b15e7ff637d.jsonl","cwd":"<home>/Developments/ForTest/agent-bridge","hook_event_name":"Stop","model":"gpt-6-astra","permission_mode":"bypassPermissions","stop_hook_active":false,"last_assistant_message":"CODENAME=BRAVO\nARRIVED_AS=The codename is BRAVO."}
{"type":"agent-turn-complete","thread-id":"01a1aa60-5d71-7220-a929-7b15e7ff637d","turn-id":"01a1aa61-a883-7540-9627-206d9a7e3ecf","cwd":"<home>/Developments/ForTest/agent-bridge","client":"codex-tui","input-messages":["[Agent Bridge native delegation]\nSource: external\n\nThis is a measureme…[839 characters cut]","The codename is BRAVO."],"last-assistant-message":"CODENAME=BRAVO\nARRIVED_AS=The codename is BRAVO."}
```

Bridge settled the request from the bound turn's `task_complete` and bound nothing to the
second turn: that message carried no delegation header, and no claim was pending. The session
stayed `ready` with one request.

## Run B: a question with no instruction about what to do next

The prompt: reply `CODENAME=<codename>`; only the caller knows whether it is `ALPHA` or
`BRAVO`; ask with `request_user_input_async`. It said nothing about waiting or ending the turn.

| step | command | result |
|---|---|---|
| 1 | `ask codex ... --prompt-file b1.md --detach --json` | `ok: true` |
| 2 | `wait <session>/<request> --questions --timeout-secs 60 --json` | after 6 s: one emission (`CODENAME?`, options `ALPHA`, `BRAVO`), `request_state: pending` |
| 3 | `result <session> --request <id> --wait --timeout-secs 120 --json` | exit 1 after 120 s, `request_state: pending` |
| 4 | `codex queue --thread <thread> --message "Answer to your question: BRAVO"` | exit 0, `Queued message 01a1aa66-4fde-7382-af59-45c6b2819dd2 for thread 01a1aa63-f040-7cd0-a69c-94072254e05f.` |
| 5 | `screen <session> --json`, 2 min 25 s after step 4 | the question with its options, `Working (4m 59s • esc to interrupt)`, the panel `Queued follow-up inputs` / `? 1 question` / `⇧← to answer` |
| 6 | `tell <session> --prompt "Answer to your question: BRAVO" --timeout-secs 20 --json` | exit 1, `session <id> is running; tell requires the ready state`; `inspect` reports `capabilities.tell`: `refused` / `state_not_ready` |
| 7 | `close-session <session> --explicit --json` | `closed: true` |
| 8 | `result <session> --request <id> --json` | `request_state: unresolved`, `session_state: closed`, `question_emitted: true`, no Result |

After the tool returned, the child called Codex's `clock.sleep` tool with 60,000 ms and
repeated it each time it returned: six calls between 06:27:25 and 06:32:39 UTC.

```json
{"timestamp":"2026-11-05T06:27:25.919Z","type":"response_item","payload":{"type":"function_call","status":"completed","name":"sleep","namespace":"clock","arguments":"{\"duration_ms\":60000}","call_id":"call_87a1b22e4ef645b9acf77f52bc2096f7"}}
{"timestamp":"2026-11-05T06:28:25.931Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_87a1b22e4ef645b9acf77f52bc2096f7","output":"Wall time: 60.0063 seconds\nSleep completed."}}
```

The queue had accepted the message of step 4 by 06:29:54. Three sleeps ended after that (06:30:31,
06:31:33, 06:32:36) and the child started the next one each time; the transcript holds no
user record after the framed prompt. The turn was still running when the session was closed
5 min 28 s after it started. The transcript has no `task_complete` or `turn_aborted` line, and
no `Stop` hook or notify for that turn was captured.

## Run C: told to end the turn after asking, then a prose question

The first prompt is run B's with one more paragraph: nobody is watching the terminal, so do
not wait after the tool call returns; end the turn with the one line
`QUESTION FOR CALLER: <the question> Options: <the options>`; the answer comes as a follow-up.

| step | command | result |
|---|---|---|
| 1 | `ask codex ... --prompt-file c1.md --detach --json`, then `wait ... --questions` | one emission (`What is the codename?`, options `ALPHA`, `BRAVO`) |
| 2 | `result <session> --request <id> --wait --json` | `request_state: completed`, Result `QUESTION FOR CALLER: What is the codename? Options: ALPHA, BRAVO`, `question_emitted: true`, `question_emission_count: 1`, 11,620 ms observed |
| 3 | `tell <session> --prompt-file b2.md --json` (body: `Answer to your question: BRAVO`) | `request_state: completed`, Result `CODENAME=BRAVO`, 14,082 ms observed |
| 4 | `tell <session> --prompt-file b3.md --json` (a second task: reply `COLOR=<color>`, GREEN or VIOLET, ask in plain prose and use no question tool) | `request_state: completed`, Result `What is the color: GREEN or VIOLET?`, 8,132 ms observed, no question fields |
| 5 | `tell <session> --prompt-file b4.md --json` (body: `Answer to your question: VIOLET`) | `request_state: completed`, Result `COLOR=VIOLET` |
| 6 | `result <session> --request <first id> --json` | the first request still holds its emission |
| 7 | `close-session <session> --explicit --json` | `closed: true` |

Each `tell` became a new turn that the queue started, in the same thread, and the answer is an
ordinary framed prompt:

```json
{"timestamp":"2026-11-05T06:33:11.814Z","type":"event_msg","payload":{"type":"task_complete","root_turn_id":"01a1aa69-35e0-7291-86cf-c4eac3660453","turn_id":"01a1aa69-35e0-7291-86cf-c4eac3660453","last_agent_message":"QUESTION FOR CALLER: What is the codename? Options: ALPHA, BRAVO","started_at":1793860384,"completed_at":1793860391,"duration_ms":7583,"time_to_first_token_ms":4890}}
{"timestamp":"2026-11-05T06:33:22.920Z","type":"event_msg","payload":{"type":"task_started","turn_attribution":{"turn_id":"01a1aa69-7ed9-78f2-80c6-9ff39752df5f","turn_trigger":"queue","parent_turn_id":null,"initiating_agent_path":null,"root_turn_id":"01a1aa69-7ed9-78f2-80c6-9ff39752df5f"},"turn_id":"01a1aa69-7ed9-78f2-80c6-9ff39752df5f","root_turn_id":"01a1aa69-7ed9-78f2-80c6-9ff39752df5f","started_at":1793860402,"model_context_window":258400,"collaboration_mode_kind":"default"}}
{"timestamp":"2026-11-05T06:33:23.044Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"[Agent Bridge native delegation]\nSource: external\n\nAnswer to your question: BRAVO\n\n[Agent Bridge Codex turn metadata; do not include this metadata in the response]\n<!-- agent-bridge-codex-turn:27583-1793860392915597000-0 -->"}],"internal_chat_message_metadata_passthrough":{"turn_id":"01a1aa69-7ed9-78f2-80c6-9ff39752df5f","create_time":1793860403.044428,"content_item_kinds":["user.text"],"content_item_metadata":[{"provenance":{"type":"user"}}]}}}
```

Nothing in the answer's records refers to the question: no call id, no reply wrapper, only
the same thread and the earlier turn's text in the conversation.

## Probe D: refusing the tool with a `PreToolUse` hook

A second temporary build, SHA-256
`26288f3b9c806558f8bd3b461b4838068e6299a3c7fa2a4c02a7c25b692c5015`, added a third event to the
injected `-c hooks=` value: a `PreToolUse` group without a matcher that runs the same
`native-hook codex` command, with its `trusted_hash` under the session-flags key for
`pre_tool_use` computed like the two existing ones. For `tool_name: "request_user_input_async"`
the hook printed a `permissionDecision: "deny"` with a fixed reason (no person is at the
terminal; do not wait or ask again; end the turn with
`QUESTION FOR CALLER: <question> Options: <option labels>`; the caller answers in a
follow-up). For any other tool it printed nothing. This build was never committed either.

Run B's prompt was used unchanged.

| step | command | result |
|---|---|---|
| 1 | `ask codex ... --prompt-file b1.md --detach --json` | `ok: true`; the launch reached the turn and the added hook ran, so Codex accepted its trust hash without a hook-review picker |
| 2 | `wait <session>/<request> --questions --timeout-secs 25 --json` | after 16 s: `ended` with `request_state: completed`; no question emission |
| 3 | `result <session> --request <id> --wait --json` | Result `QUESTION FOR CALLER: Codename? Options: ALPHA, BRAVO`, 18,655 ms observed, no question fields |
| 4 | `screen <session> --json` | `Blocked by hook` with the reason, then the `QUESTION FOR CALLER:` line; no question panel |
| 5 | `tell <session> --prompt-file b2.md --json` | Result `CODENAME=BRAVO` |
| 6 | the prose task and its answer, as in run C | Results `Is the color GREEN or VIOLET?` and `COLOR=VIOLET` |
| 7 | `close-session <session> --explicit --json` | `closed: true` |

The hook's stdin for the call, and what the model received instead of the tool's result:

```json
{"session_id":"01a1aa6c-ab5a-7503-bb74-98e67fb9c755","turn_id":"01a1aa6c-ae23-72e2-b3a5-be7ccf582137","transcript_path":"<home>/.codex/sessions/2026/11/05/rollout-2026-11-05T15-36-50-01a1aa6c-ab5a-7503-bb74-98e67fb9c755.jsonl","cwd":"<home>/Developments/ForTest/agent-bridge","hook_event_name":"PreToolUse","model":"gpt-6-astra","permission_mode":"bypassPermissions","tool_name":"request_user_input_async","tool_input":{"questions":[{"title":"Codename?","options":["ALPHA","BRAVO"]}]},"tool_use_id":"call_4a03bcac76544d50b9fbbba3065b36aa"}
{"timestamp":"2026-11-05T06:37:00.168Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_4a03bcac76544d50b9fbbba3065b36aa","output":"Tool call blocked by PreToolUse hook: No person is at this session's t…[246 characters cut]"}}
{"timestamp":"2026-11-05T06:37:05.670Z","type":"event_msg","payload":{"type":"task_complete","root_turn_id":"01a1aa6c-ae23-72e2-b3a5-be7ccf582137","turn_id":"01a1aa6c-ae23-72e2-b3a5-be7ccf582137","last_agent_message":"QUESTION FOR CALLER: Codename? Options: ALPHA, BRAVO","started_at":1793860611,"completed_at":1793860625,"duration_ms":14045,"time_to_first_token_ms":4980}}
```

The cut text is the rest of the reason followed by `. Tool: request_user_input_async`. The
hook's `turn_id` equals the bound turn's id, and `tool_use_id` is the id the function call
carries in the transcript. A refused call leaves no async `AgentMessage` item, so the
transcript observer has no emission to record.

This matches Codex's source at tag `rust-v0.166.0`:
`core/src/tools/handlers/request_user_input_async.rs` emits the item and returns
`{"accepted":true}`, and in `core/src/tools/registry.rs` the default `pre_tool_use_payload`
gives every function tool a `PreToolUse` payload, and a blocked call answers the model with
the hook's message.

## Answers to the four points

1. **The turn does not end when the question is emitted.** The tool returns
   `{"accepted":true}` at once and the model goes on. With other work to do, the child did
   that work (A). With nothing else to do and no instruction, it kept the turn open and slept
   in 60-second steps for more than five minutes, until the session was closed (B). Told to
   end the turn, it ended it within seconds (C). While the turn runs the TUI shows the
   question and a `? 1 question` panel, `wait --questions` returns the emission, `result`
   reports `pending` with `question_emitted: true`, and `tell` is refused.
2. **A queued message does not join the running turn.** `codex queue` accepts it, and it waits
   for the turn to end: six model requests passed in A and three in B without it. In A it then
   started the next turn (`turn_trigger: "queue"`); its `user.text` record and its
   `UserPromptSubmit` payload carry the new turn's id, and `Stop` and notify follow for that
   turn. In B the turn never ended, so the message was never delivered.
3. **After the turn has ended, an ordinary `tell` continues from the question.** It is a new
   Request and a new turn in the same thread. Nothing in Codex's records ties the answer to
   the question.
4. **A prose question ended the turn by itself** (C and D), and its answer went the same way.
   The difference is on the asking side: with the tool, whether the turn ends depends on what
   the child does next.

## What follows for #342

The explicit signal that #340 added for Claude is not enough for Codex. In run B the request
stayed pending for as long as the child waited, the route `tell` uses did not reach the
waiting turn, and the run ended only because the session was closed. By this measurement,
allowing `tell` during a pending request would not change that: the message would wait behind
the same turn.

Codex needs its own handling, and probe D shows one that the installed version supports: an
injected `PreToolUse` hook that refuses the tool with a reason. Its payload carries the turn
id, the call id and the questions, so a refusal could be attributed to the bound turn without
the stream correlation the Claude fallback needs. A prompt-only alternative is run C: the
caller tells the child to end its turn with the question.

`capabilities.questions.response` for Codex stays `unsupported`. Whether to build the hook,
its default, and its record and signal are decisions for a separate change.

## Not verified

- Native Windows, and any Codex version or model other than 0.166.0 with `gpt-6-astra`.
- Repeats: every case ran once. How often a child waits as in run B, and for how long, is not
  measured; the run was stopped by closing the session.
- The probe beyond one question: a `PreToolUse` group with a matcher, a user's own
  `PreToolUse` hook beside it, a drifted trust hash, a second question in one turn, a
  subagent's question, a child that ignores the reason, and an unbound or notify-settled
  request. The probe's hook also ran for no other tool, because the child called none.
- Answering at the surface (the TUI's `⇧←`), which the issue leaves out, and what a queued
  message does when a person answers there first.
- A framed Bridge prompt queued during a running turn. The manual messages were plain text.
- What Codex does with the message queued in run B after the session was closed.
- Side observation in run A, not followed up: 24 s after the queue call the injected hook
  also received a `UserPromptSubmit` payload from Codex's own memory-consolidation thread
  (another `session_id`, cwd `<home>/.codex/memories`, `transcript_path: null`). The request's
  binding was not affected.
