# Question-aware wait verification

This is the wait-notification step of #189, on the 0.4.25 release commit
`767847981044c1378e623546e55fb8ab4173a52b`. It reuses the existing Codex Question emissions.
It adds no answer transport, provider question lifecycle, or unsolicited caller notification.

## Regression and automated checks

Before implementation, the compiling adapter test
`question_wait_returns_before_completion_without_retiring_the_request` failed with
`unknown wait option: --questions`. Its fixture uses a real Claim, Receipt, injected
`UserPromptSubmit` binding, accepted synthetic rollout and the real Codex Observer. After
implementation, the public wait parser and query return the retained question while the
Request remains pending. The entire fixture tree is compared before and after each query.

Six adapter tests cover the pending return, repeated observation, skipping a seen prefix,
multiple new items, opaque ids containing slashes, cursor scope, missing and unreadable
evidence, incomplete prefixes, completion and unresolved-close precedence, input order
across sessions, and validation of a later invalid address before a selected question is
returned. The parser test rejects cursors without opt-in, duplicates, empty ids, and cursors
for addresses outside the wait. Existing ordinary-wait and excluded-query tests remain in
place. No production provider adapter or completion record changed.

Native Windows checks passed:

- `cargo test --all-targets --all-features -- --test-threads=1`: 1,292 passed, 5 ignored.
- `cargo clippy --all-targets -- -D warnings`.
- All-feature Clippy for `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`.
- `cargo fmt -- --check`, `git diff --check`, and `python scripts/check_language.py`.
- Final focused checks: 50 query tests, 6 question-wait adapter tests and the command-routing
  test, all serial. A concurrent rebuild initially encountered Windows' lock on the running
  test executable; the final focused checks ran after that full suite ended.

These local results do not establish a Windows CI result. Cross-target Clippy checks
compilation, not provider or terminal runtime.

## Authenticated native Windows check

The candidate was built with `cargo build --locked`, reported `tabcli 0.4.25`, and had SHA-256
`9ce445cd3193469591617ce72b7a8eea0df5bd5c951dad70686c57733fb6e440`.
It launched Codex CLI 0.164.0 in `windows-console` with `--yolo`, an existing trusted
workspace and a prompt file. No model or effort override was passed; the bound rollout
reported `gpt-6-astra` at `xhigh`.

The controlled prompt requested one actual `request_user_input_async` call containing an
options question and a free-text question, followed by a 25-second shell delay and a fixed
final answer. It explicitly prohibited delegation and repository edits. The delay made a
mid-turn observation possible; it was not a timing or notification guarantee.

- `wait ADDRESS --questions --json` returned both exact titles while `request_state` was
  `pending`, with `ended: null` and the Request still in `remaining`.
- At that observation, the bound rollout contained no terminal record for the turn. After
  close, all 43 rollout lines parsed successfully; the question item preceded `task_complete`
  by about 33 seconds.
- Human wait output showed the titles, options, pending state and reusable cursor.
- A one-second wait with that cursor timed out, without returning the question again. An
  ordinary one-second wait also timed out. The whole Claim token was unchanged.
- Ordinary `result --request --wait` completed with the expected answer. A later question-aware
  wait with the cursor returned the completed Result and no duplicate emission.
- Explicit close succeeded and reported no open sessions started from the fixture.

The test did not answer a question or infer that one was pending. Raw local evidence was
kept outside the checkout's build output. macOS authenticated behavior, other providers'
questions, unprompted tool use, answer routing and provider acknowledgement remain unverified.
