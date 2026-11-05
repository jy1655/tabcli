# Codex question notice on macOS, 2026-11-05

Owner live check of the question notice (#342) on one Mac: two requests with the notice on
(the default), the follow-up answers, and one request with it off. It does not cover native
Windows, another Codex version or model, a user's own `PreToolUse` hook, more than one
question in a turn, or a child that ignores the notice. Each case ran once.

The measurement that led to the notice is in
[the question-answer record](2026-11-05-macos-342-codex-question-answer.md).

## Environment

- macOS 26.5.2 (25F84), Apple Silicon, iTerm2 3.7.7, codex-cli 0.166.0, model `gpt-6-astra`
  (the configured default) at `--effort medium`. The screen was unlocked.
- Debug build of branch `feat/codex-question-notice` at `643a471`
  (`cargo build --all-features`), SHA-256
  `de074d4f851a8e916fd1eaac3852e49b75b5dbadab6d7046df52ec0612852e41`. It was copied out of
  `target/` and run by its absolute path with `AGENT_BRIDGE_NATIVE_STATE_DIR` pointing at an
  empty private directory, so the installed release and the ordinary state root were not
  involved.
- Workspace: the repository checkout, already trusted by Codex. Options:
  `--terminal iterm2 --yolo --effort medium`. Every task prompt told the child not to read or
  modify files.

## Notice on (default)

`settings` in the empty state root reported `codex_question_notice: "on"`.

First request: reply `CODENAME=<codename>`; only the caller knows whether it is `ALPHA` or
`BRAVO`; ask with `request_user_input_async`. Nothing else to do, and no instruction about
waiting or ending the turn. This is the prompt that left the request pending for more than
five minutes in the earlier measurement.

| step | command | result |
|---|---|---|
| 1 | `ask codex ... --detach --json` | `ok: true` |
| 2 | `wait <session>/<request> --questions --timeout-secs 60 --json` | after 8 s: `ended: null`, `request_state: pending`, one emission (`Codename?`, options `ALPHA`, `BRAVO`) |
| 3 | `inspect <session> --json` | `capabilities.questions.response`: `{"state":"answer_after_turn","reason":"hook_configured_unverified","answer_by":"tell"}`; `emission_observation`: `configured` |
| 4 | `result <session> --request <id> --wait --json` | `request_state: completed`, Result `QUESTION FOR CALLER: Codename? Options: ALPHA, BRAVO`, `question_emitted: true`, `question_emission_count: 1`, no deflection fields, 13,911 ms observed |
| 5 | `tell <session> --prompt-file answer.md --json` (body: `Answer to your question: BRAVO`) | `request_state: completed`, Result `CODENAME=BRAVO` |
| 6 | `doctor <session> --probe --json` | `codex_hook_trust`: three hooks, each `trustStatus: "trusted"` with `hash_matches: true`, the third under `/<session-flags>/config.toml:pre_tool_use:0:0` |
| 7 | `close-session <session> --explicit --json` | `closed: true` |

Session records: `codex-question-notice.json` held
`{"schema":1,"notice_hook":"injected","reason":null,"vector_codex_version":"0.166.0"}`.
In Codex's transcript the notice is a developer message of kind `hooks.additional_context`
written 33 ms after the function call, the async `AgentMessage` item with the question follows
it, and `task_complete` comes 4 s later with the `QUESTION FOR CALLER:` line. The child called
no sleep tool.

Second request, in a new session: ask for the codename with the tool and, independently of the
answer, run `sleep 20` once and compute 17 * 23; report `PRODUCT=<the product>` and the
codename if known.

| step | command | result |
|---|---|---|
| 1 | `ask codex ... --detach --json`, then `wait ... --questions` | after 10 s: `request_state: pending`, one emission (`What is the codename?`, options `ALPHA`, `BRAVO`) |
| 2 | `result <session> --request <id> --wait --json` | `request_state: completed`, Result `PRODUCT=391` and, after a blank line, `QUESTION FOR CALLER: What is the codename? Options: ALPHA, BRAVO`; `question_emitted: true`; 37,604 ms observed |
| 3 | `tell <session> --prompt-file answer.md --json` | `request_state: completed`, Result `PRODUCT=391` / `CODENAME=BRAVO` |
| 4 | `close-session <session> --explicit --json` | `closed: true` |

The transcript holds one notice message, at the question call. The three shell calls that
followed it (the `sleep 20` and two polls of it) have none, so the matcher kept the hook to the
question tool.

What this shows for codex-cli 0.166.0 on macOS: Codex accepted the injected `PreToolUse` group
with its matcher and trust hash (the launch reached the turn, no hook-review picker); the hook
added context without stopping the tool, so the question was emitted and observed while the
request was pending; the child did not wait in its turn; work that did not depend on the answer
was finished first; and the answer sent with an ordinary `tell` continued the task.

## Notice off

After `settings codex-question-notice off` in the same state root, the first prompt again:

| step | command | result |
|---|---|---|
| 1 | `ask codex ... --detach --json`, then `wait ... --questions` | after 7 s: `request_state: pending`, one emission |
| 2 | `result <session> --request <id> --wait --timeout-secs 75 --json` | exit 1 after 75 s, `request_state: pending` |
| 3 | `inspect <session> --json` | `capabilities.questions.response`: `{"state":"unsupported","reason":"no_addressed_answer_path"}` |
| 4 | `screen <session> --json` | the question with its options, `Working (1m 20s • esc to interrupt)`, the panel `Queued follow-up inputs` / `? 1 question` |
| 5 | `close-session <session> --explicit --json` | `closed: true` |

Session records: `codex-question-notice.json` held
`{"schema":1,"notice_hook":"not_injected","reason":"settings_off","vector_codex_version":"0.166.0"}`.
The transcript has no notice message, and the child called `clock.sleep` for 60,000 ms twice
before the session was closed. Without the notice the behavior is the one measured earlier.

## Not verified

- Native Windows, where the hook value has its own quoting and state key and the pinned hash is
  computed only; no Codex on Windows has reported it.
- Codex versions other than 0.166.0 and models other than `gpt-6-astra`. The notice is advice:
  another model, or the same one on another task, may still wait.
- A user's own `PreToolUse` hook for the tool, a second question in a turn, a subagent, an
  unbound or notify-settled request, and an unreadable notice record in a live session.
- The hook's own stdout and stdin in these runs. No payload was captured with this build; the
  evidence is Codex's transcript, the settled requests and the records above. A temporary build
  that recorded the payloads showed the same exchange earlier the same day, with a group that
  had no matcher.
- Focus and selection changes in the terminal during the launches.
