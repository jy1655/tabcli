# Claude question deflection on macOS, 2026-11-05

Owner live check of #340 on one Mac. It covers one question in the bound turn with
deflection on, the follow-up answer, and the same request with deflection off. It does not
cover native Windows, a user's own `PreToolUse` hook, a subagent's or a plugin's question,
a retried or broken step, or more than one question in a turn.

## Environment

- macOS 26.5.2 (25F84), Apple Silicon, iTerm2 3.7.7, Claude Code 2.1.318.
- Debug build of branch `feat/340-claude-question-deflection` at `cc6b4e8`
  (`cargo build --all-features`), SHA-256
  `b01b3b653a31f4083a1b79c0c92f22e9208c99ad8385ca93112a8a21899c280e`. It was run by its
  absolute path with `AGENT_BRIDGE_NATIVE_STATE_DIR` pointing at an empty private directory,
  so the installed release and the ordinary state root were not involved.
- Workspace: the repository checkout, already trusted by Claude. Options:
  `--terminal iterm2 --model opus --yolo`.

The prompt told the child that it needed one fact only the caller has (which of two baseline
directories to compare, `docs` or `tests`), to ask for it with `AskUserQuestion` with exactly
those two options and not in prose, and to reply `Baseline chosen: <answer>` once it had the
answer.

## Deflection on (default)

`settings` in the empty state root reported `claude_question_deflection: "on"`.

| step | command | elapsed | result |
|---|---|---|---|
| 1 | `ask claude ... --timeout-secs 180 --json` | 23 s | `ok: true`, `request_state: completed`, `question_emitted: true`, `question_emission_count: 1`, `question_deflected: true`, `question_deflection_count: 1`; Result: `QUESTION FOR CALLER: Which baseline directory should this check compare? Options: docs, tests` |
| 2 | `result <session> --request <id> --json` | | same counts; `question_emissions` holds one item `step:0/tool:0` with `deflected: true`, the question title and options `docs`, `tests`; no `question_observation` |
| 3 | `inspect <session> --json` | | `capabilities.questions.response`: `{"state":"deflected_to_result","reason":"plugin_configured_unverified","answer_by":"tell"}`; `capabilities.tell.state`: `admissible` |
| 4 | `tell <session> --prompt-file answer.md --json` (body: `Answer to your question: tests`) | 12 s | `ok: true`, `request_state: completed`; Result: `Baseline chosen: tests` |
| 5 | `close-session <session> --explicit --json` | | `closed: true` |

Session records: `claude-question-mode.json` held `{"schema":1,"deflection":"on"}`, the
module's `config.js` held `export const deflect = true;`, and the question record held the
one item with `"deflected": true`, `incomplete: false`, bound to the request's turn.

What this shows for Claude Code 2.1.318 on macOS: the loader accepted the module with its
`./config.js` import; the `tool.call` hook saw the call with engine/core origin and a
`tool_use_id` equal to the id of the tool chunk the `turn.step` hook had passed; the refusal
text made the child end its turn with the question and keep the turn protocol's marker, so
native completion settled the request; and the answer sent with an ordinary `tell` continued
the task. Whether the call arrived while the step was still streaming or after it was not
recorded: both paths end in the same record.

## Deflection off

After `settings claude-question-deflection off` in the same state root, the same prompt:

| step | command | result |
|---|---|---|
| 1 | `ask claude ... --timeout-secs 45 --json` | exit 1, `failure_kind: timed_out`, `request_state: pending` after 45 s |
| 2 | `result <session> --request <id> --json` | `request_state: pending`, `question_emitted: true`, `question_emission_count: 1`, no deflection fields, one item `step:0/tool:0` without `deflected` |
| 3 | `inspect <session> --json` | `capabilities.questions.response`: `{"state":"unsupported","reason":"no_addressed_answer_path"}` |
| 4 | `claude agents --json` | the session listed with `status: "waiting"`, `waitingFor: "input needed"` |
| 5 | `close-session <session> --explicit --json` | `closed: true` |

Session records: `{"schema":1,"deflection":"off"}` and `export const deflect = false;`. The
dialog path is unchanged: the question is observed, nothing is refused, and the request
stays pending until someone answers at the surface or the session is closed.

## Re-check on the final change

A later fix made the record compare the refused call's question text and option labels with
the retained item (`e836ddd`). The deflection-on request was repeated with a debug build of
that commit's tree, SHA-256
`e28f7daade557d092970a371a73ed5ffade7fa5d380d6a8ac6cae53beef9688d`, in another empty state
root: `ask` completed in 19 s with the same `QUESTION FOR CALLER:` Result,
`question_deflected: true` and `question_deflection_count: 1`; the record held
`step:0/tool:0` with `"deflected": true` and `incomplete: false`. So the questions that
`tool.call` carries project to the same text and labels as the step's tool use in a live
session. The follow-up `tell` and the deflection-off request were not repeated.

## Not verified

- Native Windows, including a Windows `reopen` reading the setting again.
- A user's `PreToolUse` hook for `AskUserQuestion`, managed-settings denials and permission
  modes other than the bypass used here.
- A subagent's question, a plugin's own `$.ui.ask`, a second question in the same turn, a
  repeated dispatch of one call, and a step whose stream breaks after a refusal. The module
  harness covers the last three; no live session did.
- What the terminal showed at the moment of the refusal. No screen was read; the evidence is
  the settled request and the records above.
- Codex, Agy and Pi are unaffected by construction and were not run for this change.
