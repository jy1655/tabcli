# Bug triage — 2026-10-01

Source baseline: `9d2cd61c12a9bb62e7f98da409362f074969ffa3`, the unpublished
0.0.9 candidate. Work branch: `fix/codex-follow-up-target-20261001`.
This record describes local changes and tests, not a release or installed update.

## Scope and disposition

The eight open issues were inspected. #56 and #48 describe reported failures;
#57, although labelled enhancement, identifies a wrong-target input path.
#6, #28 and #53–#55 remain feature work outside this patch.

### #57 — reject unaddressed Codex follow-up

Before the fix, the deterministic `unavailable_queue_never_selects_unaddressed_terminal_input`
test failed with `TerminalFallback` instead of `ReturnError`. It establishes a
managed thread A, makes the provider reject queue input before enqueue, and
checks the shared dispatch decision. Stored A identity cannot establish whether
the TUI still shows A, has switched to B, or is displaying the agent picker.
`terminal_follow_up_refuses_before_installing_pending_turn_metadata` also failed
because the old adapter prepared a paste without active-thread evidence.

The Codex adapter now converts known queue unavailability into a pre-send refusal.
Its terminal preparation and send entrypoints also refuse unaddressed follow-up.
The existing UUID-addressed queue, accepted/uncertain classification, completion
correlation and retained uncertain claims are preserved. Tests cover removal of
pending metadata after a known queue rejection, claim release, retained request
identity, and admission of a later independently requested turn.

`doctor` reports `codex_terminal_follow_up=unavailable`. This changes compatibility:
Codex `tell` needs the existing native queue prerequisites, including a compatible
local daemon; older versions/platforms without them retain `ask` but lose terminal
follow-up. No shared transport or Claude/Agy/Pi adapter was modified.

### #56 — MCP authentication remains unresolved

A fresh baseline Bridge session (`session-fkWAA9`) completed both model turns.
A read-only query of Codex's local log database, scoped to that thread's process
UUID, separately found `codex_apps`'s `Service initialized as client` record
(log row `135091504`, process `pid:49789:e9e4d347-11a4-4d07-b8d8-9bd16d5d01c7`).
Its startup request used the `in-process` backend. This is positive initialization
evidence for this run, not a claim that the user's recurring `401/token_revoked`
error has been fixed.

`doctor` now explicitly reports `codex_mcp=unknown`: neither a completed model
turn nor daemon/version checks prove MCP connection or authentication success.
The command does not read credentials, connect MCP, refresh authentication,
restart the daemon, disable apps, or resend input. The
[official app-server documentation](https://learn.chatgpt.com/docs/app-server)
defines per-server startup notifications and `reauthenticationRequired`;
the adapter does not yet ingest those notifications for its interactive target.

Direct interactive Codex comparison, failed startup capture, and active `/agent`
switching remain **NOT-VERIFIED**. Computer Use denied access to iTerm2; that UI
restriction was not bypassed. The original issue remains open.

### #48 — original Agy missing receipt remains unresolved

The installed Agy is now **1.2.14**, whereas the report concerns 1.2.12.
Terminal.app session `session-0M5Vpr` completed initial/follow-up/explicit close;
the follow-up took 64.04 seconds including the readiness gate and recorded a
confirmed input receipt. This does not reproduce or explain the original loss.

iTerm2 session `session-JIVYpv` did not complete its initial turn. Agy logged
`RESOURCE_EXHAUSTED (code 429): Individual quota reached`, then later
`Terminal gone, shutting down`. It therefore supplies no follow-up comparison.
No Agy timing window, receipt requirement, retry behavior or adapter was changed.

## Manual checks of the final code

Host: macOS 26.5.2 arm64; Codex CLI 0.159.3; iTerm2. Prompts required the same
marker in both turns and prohibited tools, edits and delegation.

| Case | Evidence | Result |
| --- | --- | --- |
| Normal Codex native queue | `session-p9CieC`; initial request `request-1790836601378264000-92287-0`, follow-up `request-1790836613770629000-94416-0` | Exact results, same provider thread, doctor and explicit close passed |
| Controlled unavailable queue prerequisite | `session-752jWb`; follow-up `request-1790836633020465000-96202-0` | Refused before terminal input; session returned `ready`, no claim, only initial completion event; explicit close passed |

The second case used a task-local executable shim that forwarded Codex startup
unchanged but returned `status=stopped` only to `app-server daemon version`.
The real shared daemon was not stopped or changed. This is a controlled gate
test with an authenticated target, not a real daemon outage or UI-switch test.
The existing result contract reports this non-completed request as `unresolved`
alongside the explicit refusal; it is not a completion or delivery-uncertain claim.

All five task-owned sessions were audited `closed`, without active claims or
terminal ownership records. Windows runtime and agent-picker UI remain
**NOT-VERIFIED**. Ignored live tests are not runtime evidence.

## Automated checks

| Check | Result |
| --- | --- |
| `cargo test --all-targets --all-features -- --test-threads=1` | 440 passed, 0 failed, 4 manual LIVE tests ignored |
| `cargo clippy --all-targets -- -D warnings` | Passed |
| `cargo fmt -- --check` | Passed |
| `git diff --check` | Passed |

The test harness ran serially after the manual sessions were closed. These are
host checks; no Windows runtime or cross-target result is claimed.

Final required checks are recorded in the task's private evidence directory:
`/private/tmp/agent-bridge-bugs-20261001-k3dl150r`.
It contains request/result JSON, sanitized MCP evidence, provider-owned Agy logs,
session cleanup audit and `check-*.log` files. It is temporary local evidence.
