# Bug triage — 2026-10-01

Source baseline: `9d2cd61c12a9bb62e7f98da409362f074969ffa3`, the unpublished
0.0.9 candidate. Work branch: `fix/codex-follow-up-target-20261001`.

The work ran in two rounds on the same day. Round 1 is commit
`4aa60f49325efc0901054a39aef413a1bf9b9e8c`: it fixed #57 in the Codex adapter,
made `doctor` report `codex_mcp=unknown`, and left #56 and #48 unresolved.
Round 2 is the change that carries this record. It found and fixed the cause of
#48, fixed one more defect in the candidate's Agy trust-dialog matcher, made a
turn that Agy gives up on fail at once, classified the cause of #56 from
provider logs, and completed the live checks of #57.

This record describes source changes and tests. It is not a release, a tag, or
an installed update. The installed binary is still 0.0.8.

## Scope and disposition

The eight issues that were open at the start were inspected, and one was opened
during the work. The table states the outcome for each.

| Issue | Outcome on this branch | State of the issue |
| --- | --- | --- |
| #57 Codex follow-up target | Fixed in round 1. Round 2 verified it with a modal picker open in the managed TUI. | Open until the owner closes it; switching to a second live agent thread is not verified. |
| #48 Agy follow-up receipt | Cause found and reproduced. Fixed in round 2 and verified on iTerm2 and Terminal.app. | Open until the owner closes it; native Windows is not re-verified. |
| #56 `codex_apps` 401 `token_revoked` | Cause classified from provider logs as an invalidated ChatGPT sign-in. Not reproducible after the user signed in again. `doctor` guidance improved. | Open; the session in which the user saw the error was not identified. |
| Agy quota failure (no issue; asked for during round 2) | A turn Agy gives up on is recorded as a failed request at once instead of waiting out the timeout. | Verified on macOS. |
| #58 new managed sessions take the keyboard focus (opened during round 2) | Investigated only: the launch scripts call `activate`, and no supported terminal can create a surface without selecting it. No change here. | Open. |
| #6, #28, #53, #54, #55 | Feature work, outside this patch. | Unchanged. |

## #48 — the follow-up paste landed on Agy's workspace-trust dialog

Agy keeps its workspace-trust dialog over the composer until the exact
workspace is in its trust store. It still runs an initial prompt that was
delivered as a launch argument behind the dialog, so on macOS `ask` returns a
result while the dialog is still on screen.

A paste onto the dialog is discarded. Its Enter confirms the preselected option
`Yes, I trust this folder`. Agy then logs the trust reload: one `Reloading
system slash commands and skills` line and three companion lines, 356 bytes in
total, and no receipt. The original report quoted exactly that: no receipt in
the 356 bytes appended after the pre-paste offset.

The line that earlier notes called the "deferred skills reload" is this trust
reload. It is caused by the paste. It is not a startup timer, so no waiting
window can prevent the loss. The old behaviour also had a side effect: the lost
paste approved a workspace that no one had approved.

## Evidence for the cause

1. Agy 1.2.14 binary. A direct-call scan of the binary's function table found
   three callers of `store.(*Manager).ReloadSlashCommandsAndSkills`, the
   function that logs the line: `RootModel.Init.func4` (startup),
   `Manager.SwitchToConversation` (a new or switched conversation), and
   `Manager.reloadWorkspaceCustomizations`, which is called by
   `AddWorkspaceDir`, which `TrustWorkspace` calls. Indirect calls are not
   covered by the scan.
2. Agy logs on this Mac, 2026-09-10 to 2026-10-01. In the first 86 logs a late
   reload (later than 1 s after startup and not right after a conversation
   start) appears in 27 sessions, and every one of those workspaces is in the
   trust store. It appears in none of the 25 sessions whose workspace is not in
   the trust store. The other 34 sessions ran in workspaces that were already
   trusted and never logged it. There is no counterexample, and the 26 logs
   added during round 2 agree.
3. Reproduction on demand, Agy 1.2.14, iTerm2, baseline binary `4aa60f4`,
   isolated state directory, a new workspace:

| Step (session-U2yPxX) | Observation |
| --- | --- |
| `ask` | Result `AGY48_INITIAL_OK`; the dialog still on screen; workspace absent from the trust store |
| Paste with the adapter's own iTerm2 script (file contents, then Return) | Sent at 16:29:08 |
| Agy log after the paste | Four lines at 16:29:08.527, 356 bytes: the trust reload; no receipt |
| Screen and trust store after the paste | Dialog gone, composer empty, workspace added to the trust store |

The same session then accepted a real `tell` in 7 seconds, because the
workspace was now trusted.

## Why earlier attempts did not reproduce it

- Each loss approved the workspace as a side effect, so every later run in the
  same workspace had no dialog and passed.
- Comparison runs used workspaces that were already trusted. There the old gate
  only waited its full 60 seconds and then pasted successfully, which looked
  like an intermittent problem.
- On 2026-09-24 the reload followed each lost paste by 0.3 to 1.4 seconds
  whatever the waiting window was (12, 20, 35 seconds), and a session that
  pasted nothing never logged it. That pattern was read as a timer.
- During round 2 the dialog of the first test session (session-KbgbIY) was
  approved from the keyboard 20 seconds after it opened, by a keypress that the
  task did not send. Agy logged the trust reload at 16:27:49.887 and the
  following `tell` passed. Human approval produces the same reload line.

## The fix for #48

The change is confined to the Agy adapter. A paste now needs two facts about
workspace trust, and the time window is gone on macOS.

- Trust store: Agy's own trust store lists the exact workspace. A parent entry
  does not count, and an unreadable store is not an approval.
- This session's own log: `agy.log` shows that this Agy process loaded the
  workspace customizations, a `hooks_manager.go` line logged by a goroutine
  other than the main one. Agy logs it at startup when the workspace was
  already trusted, or when the dialog is approved in that process; the main
  goroutine logs the only other such line while the store manager is built.
  The second fact is needed because the store is shared by every Agy process
  (see the review finding below).
- The macOS follow-up and the native Windows initial paste both wait for the
  two facts. If the request deadline passes first, the request fails as
  `not_sent` before any terminal input: on macOS the session returns to
  `ready`, the claim is released, and no pending-turn file remains. A human can
  approve the dialog in the managed terminal during the wait. The error text
  starts with `Agy workspace trust was not verified before the deadline` and
  names the missing fact.
- A session log without the load line withholds the paste but does not prove
  an open dialog: the log can be missing, or cut before the line, in a session
  that has no dialog. The error therefore states what was read, and its
  recovery depends on what the managed terminal shows: approve the dialog if
  it is there, otherwise close the session and start a new one.
- The 60-second macOS reload window is removed. After the trust evidence the
  gate needs `CLI startup completed` and one 3.5-second quiet period. The
  receipt requirement, the 60-second receipt window, claim retention for an
  unconfirmed paste and the no-resend rule are unchanged.
- `doctor <session>` has a new check `agy_workspace_trust`: `available` when
  both facts hold, `unavailable` with reason `agy_workspace_untrusted` when the
  store does not list the workspace, `unknown` with reason
  `agy_session_trust_unverified` when the store lists it but this session's
  log does not show the load, and `unknown` when the evidence cannot be read.
  It is an observation only.
- Native Windows keeps its 45-second window unchanged, because native Windows
  was not re-verified after the cause was found. With the trust evidence in
  front, that window only delays a workspace that was trusted before launch.
- No other provider adapter and no shared transport was modified for #48.

Tests added or changed in the Agy adapter:

- The four log lines of session-U2yPxX, verbatim, must classify as no receipt
  in 356 appended bytes.
- The per-process signal is asserted on the recorded logs of both platforms:
  the two delivered Windows pastes of 2026-09-24 (session-udT6uY,
  session-M8QFPp) show the load at startup; every lost paste shows it only
  after the paste; session-IQHEwf, which was never approved, never shows it.
- The trust wait is tested for a missing store, a parent-only entry, a store
  entry with a session log that shows no load, a missing session log, the
  recorded log of a trusted session cut just before its load line, an approval
  in this session, a workspace trusted before launch, an approval during the
  wait, and an unreadable store.
- The three macOS gate tests now assert one quiet period instead of the
  60-second window.

## Review finding: the trust store is shared between sessions

The first version of the fix waited for the trust store alone. A review by
Codex through Agent Bridge (session-jH9ft5) objected that the store is global:
if two Agy sessions start in the same untrusted workspace and the dialog is
approved in one of them, the store lists the workspace while the other session
still shows its dialog.

That state then occurred without being staged. Two sessions were open in one
new workspace. At 17:30:04 a keypress that the task did not send approved the
dialog of session-v8Ayr2. At 17:30:09 the store listed the workspace, the
dialog of session-UuYk87 was still on screen, and its log had no load line.
The store-only rule would have pasted onto that dialog.

The per-process log line closes the gap. It was checked against 112 Agy logs of
versions 1.2.6 to 1.2.14 on this Mac: all 77 sessions that were trusted at
startup or approved in the session have the line, none of the 35 sessions whose
dialog was never answered in that session has it, and in all 19 sessions with
a receipt the receipt follows the line.

## Later review findings: a missing load line is not an open dialog

The second and third review rounds objected to the report, not to the refusal.
The second version called every session without the load line "its own trust
dialog is still open". That is wrong for a session whose `agy.log` is missing
or was cut before the line: such a session may have no dialog, and the advice
to approve one cannot be followed. A first correction told the two cases apart
by the startup line of the log. The third round showed that this is not enough
either: the recorded log of session-udT6uY has its startup line 1 ms before its
load line, so a log cut between them looks like an open dialog.

The final version reports one state for a store entry without a load line,
"this session's trust is unverified". The paste stays withheld, `doctor`
reports `unknown`, and the recovery is conditional: approve the dialog if the
managed terminal shows one; if it shows the composer, or the prompt is still
withheld after the approval, close the session and start a new one.

The second round also confirmed a defect that had been found and fixed while
it ran. A turn failure was first bound to the pending claim by Agy's transcript
alone, so a new claim could inherit the error of the previous turn while its
own log lines were not written yet. The receipt now binds a pasted turn. The
review narrowed one more statement: a result wins only when Agy has written it
before the error is seen. The fourth round found no defect in the final tree.

## Live checks of the fix

Host: macOS 26.5.2 arm64, Agy 1.2.14, isolated state directory. Prompts asked
for a fixed marker and prohibited tools. The first table used intermediate
builds of round 2. The second table repeats the main cases with the final
source, after the review rounds.

| Case | Evidence | Result |
| --- | --- | --- |
| Untrusted workspace, iTerm2 | `session-pJvfAe`, `tell` with a 25-second timeout | Refused as `not_sent`; 0 bytes appended to `agy.log`; dialog still on screen; trust store unchanged; session `ready`; no claim |
| Same session after approval in the managed terminal | Trust reload at 16:45:45.958; paste at 16:45:51.547; receipt at 16:45:52.03 | `tell` completed in 8 seconds with the exact marker |
| Trusted workspace, iTerm2 | `session-2nvmMQ`; paste 4.41 s after `tell` started; receipt 0.42 s later | `tell` completed in 7.9 seconds |
| Trusted workspace, Terminal.app | `session-TIBx7n`; paste 3.88 s after `tell` started; receipt 0.21 s later | `tell` completed in 6.8 seconds |
| Trusted parent, untrusted child directory, iTerm2 | `session-rsH6gj`; the parent is in the trust store, the child is not | Agy showed the dialog for the child; a parent entry does not suppress the dialog |
| Two sessions in one untrusted workspace, only the first approved, iTerm2 | `session-QwXzCo` approved, `session-pl959u` not; `tell` to the second with a 25-second timeout | Refused as `not_sent` because the session's own log showed no load (that build reported it as an open dialog); 0 bytes appended; screen byte-identical. After approval there, both sessions completed `tell` in 6.7 and 6.5 seconds |
| Trusted workspace with the per-process rule, iTerm2 | `session-SkuiN6`; paste 4.41 s after `tell` started; receipt 0.56 s later | `tell` completed in 7.9 seconds; `agy_workspace_trust=available` |
| Trusted workspace with the per-process rule, Terminal.app | `session-Z2DyNL`; paste 3.88 s after `tell` started; receipt 0.31 s later | `tell` completed in 6.3 seconds |
| Untrusted workspace, Terminal.app | `session-uZYvab`, `tell` with a 20-second timeout, then approval in the managed terminal | Refused as `not_sent`; 0 bytes appended; dialog still on screen; trust store unchanged. After approval `tell` completed in 6.3 seconds |
| Shared consent from a Codex fixture, iTerm2 | `session-0QGTon`; `applied=provider-workspace-dialog` | `ask` 10.9 seconds, `tell` 6.8 seconds; `agy_workspace_trust=available` |
| Agy quota exhausted, iTerm2 | `session-aR1DnT` | `ask` failed in 11.0 seconds and `tell` in 6.3 seconds with Agy's own error |

Final source:

| Case | Evidence | Result |
| --- | --- | --- |
| Trusted workspace, iTerm2 | `session-wcN9NE`; paste 4.08 s after `tell` started; receipt 0.43 s later | `tell` completed in 7.5 seconds; `agy_workspace_trust=available` |
| Same session with its `agy.log` moved away | `tell` with a 15-second timeout; `doctor` | Refused as `not_sent`: the store lists the workspace, the log does not show the load, "its own trust dialog may still be open"; nothing pasted; `agy_workspace_trust=unknown`, `agy_session_trust_unverified`. With the log moved back `tell` completed in 6.9 seconds |
| Trusted workspace, Terminal.app | `session-9jRaFo`; paste 3.79 s after `tell` started; receipt 0.32 s later | `tell` completed in 6.0 seconds |
| Untrusted workspace, iTerm2 | `session-CsX3Rz`, `tell` with a 20-second timeout, then approval in the managed terminal | Refused as `not_sent`; 0 bytes appended; screen byte-identical; trust store unchanged; `agy_workspace_untrusted`. After approval `tell` completed in 7.9 seconds |
| Agy quota exhausted, iTerm2 | `session-zajF7K`, see the next section | `ask` failed in 11.0 seconds and `tell` in 5.4 seconds with Agy's own error; session `ready` |

Shared consent and the two-session case were not repeated with the final
source; the paste decision they exercise did not change after their runs.
Before the fix the same trusted-workspace follow-up took 64.04 seconds in
round 1 (session-0M5Vpr). All sessions were closed explicitly.

One run is recorded for completeness. In `session-RPM45T` (Terminal.app, model
GPT-OSS 120B) the paste was delivered and its receipt confirmed, but the model
shortened the turn marker in its answer, so the result was not accepted and
`tell` ran into its 180-second timeout. The exact-marker rule is unchanged.

Keys that the task did not send reached its new terminal tabs three times. Two
of them approved an Agy trust dialog (see above). The third arrived before the
launch command of `session-MG4ifx`: the tab shows `A. '<session>/launch.sh'`
and `zsh: command not found: A.`, and `ask` failed after 30 seconds with
`provider launch timed out before startup was confirmed`. A new tab takes the
keyboard focus, so a key typed at that moment lands in it. The source of the
keys was not identified. The behaviour was not changed here; it is recorded as
issue #58.

One more launch failed for a reason outside this change. While the machine was
under heavy load (load average 78) `ask` for `session-y4wuI9` gave up after 29
seconds with `iTerm2 automation timed out`. iTerm2 created the tab about 70
seconds later, so it stayed open without a session to own it and was closed by
hand.

## Agy turns that the provider gives up on

During round 2 Agy's account quota ran out. Agy then ends the turn with one
`agent executor error:` line in `agy.log`, for example `generating and
executing: RESOURCE_EXHAUSTED (code 429): Individual quota reached. ... Resets
in 2h22m28s.`, and writes no result to the transcript: the conversation keeps
only its `USER_INPUT` step. Bridge had no signal for that, so every such
request waited out its whole timeout. Fifteen sessions on this Mac did so that
day; four of them stayed open for more than 20 minutes.

The Agy result monitor now records such a turn as a failed request for the
pending claim. It does so only when the error line follows the newest
`Forwarding user message` line of the log and the log shows that turn to be the
pending one. A pasted turn is bound by the receipt that carries its marker. The
argument-delivered first turn has no receipt; it must be the first turn of the
log, and the only `USER_INPUT` step of Agy's full transcript must carry the
marker. An error of a turn typed by hand or of an older turn is therefore
ignored, and a newer claim cannot inherit the first turn's error while its own
log lines are not written yet. A result that Agy has already written is
recorded first. Once the failure is recorded the claim is released, so a result
that arrives later for that turn is not accepted. The session returns to
`ready` with the provider's text in `status.error`.

Live check with the final source and an exhausted model (`session-zajF7K`):
`ask` returned after 11.0 seconds and the following `tell` after 5.4 seconds,
each with `request_state=failed`, `session_state=ready` and the error
`Agy turn failed: generating and executing: RESOURCE_EXHAUSTED (code 429):
Individual quota reached. ...` including Agy's reset time. The follow-up was
pasted and its receipt confirmed before Agy gave up on it. Before the change
the same `ask` ended only at its 180-second timeout.

Agy offers no way to read the remaining quota without spending a model request,
so Bridge cannot report the state before a turn is sent.

## Candidate defect found while checking shared consent

The candidate's Agy trust-dialog matcher accepted a footer line only when it
started with `Gemini `. The footer shows the label of the saved model and
appears about one second after the dialog. On this Mac the saved model is
`Claude Opus 4.6 (Thinking)`.

In `session-a1QguW` consent was verified, the dialog was never answered, and
`ask` failed at its 180-second deadline with `iTerm2 automation timed out`
although the initial result had been recorded. `session-n3tsmp` passed only
because the screen was read 0.95 seconds after startup, before the footer
appeared.

The matcher now accepts the dialog alone or followed by a footer that starts
with one of the model families that `agy models` lists in 1.2.14: `Gemini `,
`Claude `, `GPT-`. Any other trailing line still leaves the dialog to the user.
The regression test covers both new footers and the screen without a footer.
After the fix `session-JaxYGc` and `session-0QGTon` answered the dialog.

One behaviour was not changed: when a dialog is not recognised, launch still
waits until the request deadline before it reports the failure. That behaviour
belongs to #28.

## #57 — reject unaddressed Codex follow-up

Round 1 is unchanged. The Codex adapter converts known native queue
unavailability into a pre-send refusal, and its terminal preparation and send
entry points refuse an unaddressed follow-up. UUID-addressed queue delivery,
accepted/uncertain classification, completion correlation and retained
uncertain claims are preserved. `doctor` reports
`codex_terminal_follow_up=unavailable`. Codex `tell` therefore needs the native
queue prerequisites, including a compatible local daemon; older versions and
platforms keep `ask` and lose terminal follow-up.

Round 2 added a live finding. A Bridge-launched Codex TUI is not connected to
the shared background server. `/agents` in that TUI opens a modal that says
`Shared agents unavailable` and offers `1. Start background server`
(preselected) and `2. Return to this session`. An Enter typed there would start
a background server. This is the kind of screen that the removed terminal
fallback could have typed into.

| Case (Codex CLI 0.159.3, iTerm2, candidate binary) | Evidence | Result |
| --- | --- | --- |
| Normal queue | `session-EHNs24`, `ask` then `tell` | Exact results on the same provider thread; `tell` took 10.9 seconds |
| Modal open, queue available | `session-EHNs24`, `/agents` modal on screen, then `tell` | Completed in 12.6 seconds with the exact marker; the modal stayed open and unchanged |
| Modal open, queue prerequisite unavailable | `session-oTgoFQ`, `/agents` modal on screen, then `tell` | Refused in 0.6 seconds before any input; screen byte-identical before and after; session `ready`; no claim; only the initial completion event |

The controlled case used a task-local executable shim that answers only
`app-server daemon version` with `{"status":"stopped"}` and forwards everything
else to the real CLI. The real shared daemon was not stopped or changed.

## #56 — `codex_apps` failed with 401 `token_revoked`

The provider's own logs record the failure and when it ended. Times are KST.

| Time (KST) | Source | Record |
| --- | --- | --- |
| 2026-09-30 23:31:11 | Codex log database | First retained `401 Unauthorized`, `token_revoked` on the model catalogue request |
| 2026-10-01 08:53:14 | `~/.codex/app-server-daemon/daemon.stderr.log` | `Failed to refresh token`, `refresh_token_invalidated`, "Your session has ended. Please log in again." |
| 2026-10-01 08:53:14 to 08:53:22 | same file | 12 `rmcp::transport::worker` fatal lines with `HTTP 401` and `token_revoked`: the MCP handshake failure the user reported |
| 2026-10-01 10:30:15 to 10:40:52 | Codex log database, desktop app-server | 79 times "your refresh token was revoked. Please log out and sign in again." |
| 2026-10-01 10:39:14 | `~/.codex/auth.json` modification time | Sign-in file rewritten |
| after 10:40:52 | Codex log database | No provider `token_revoked` or 401 record in any process; the two later matches are prompts that quote the error text |

The stored ChatGPT sign-in on this Mac was invalidated by the provider. Every
Codex surface failed in the same way until the user signed in again: the shared
daemon, the desktop app, and any newly started CLI. The desktop app-server kept
failing with its in-memory tokens until it was restarted; the shared daemon,
started at 08:53, served a new thread without errors at 17:02 without a
restart.

The launch paths differ, according to the app-server transport recorded in the
Codex logs. A Bridge-launched Codex runs its own in-process app-server, because
Bridge passes `-c notify=...`. A directly started interactive Codex attaches to
the shared daemon over a Unix socket. A Bridge session therefore reads the
stored sign-in at every launch, which matches "it repeats in new sessions"
while the sign-in is invalid. Bridge does not read, copy, refresh, or change
credentials.

| | Bridge `ask` (`session-EHNs24`, 16:59) | Direct interactive `codex` (17:02) |
| --- | --- | --- |
| App-server transport | `in-process` | `unix_socket` (shared daemon) |
| `codex_apps` client initialised | 16:59:19 | 17:02:04 |
| Auth or MCP worker errors | 0 | 0 |
| MCP warning on screen | none | none |

The official `codex doctor --json` at 16:15 reported `auth.credentials` ok
(ChatGPT tokens in file storage) and `network.websocket_reachability` ok, an
authenticated handshake with `HTTP 101 Switching Protocols`.

Bridge Codex sessions created that day before 17:00: 213 sessions; 169 have a
retained `codex_apps` client-initialised record; none has a retained failure
record; 38 have no retained startup rows and 6 have no recorded thread. The
Codex log database keeps at most 1000 process-scoped rows per process, so a
long session loses its startup rows. Absence of a record is not evidence of
failure or of success.

Not established: which session displayed the error to the user. The user pasted
the error text at 14:44. No Bridge Codex session exists in the invalid window:
the first one of the day started at 10:44:54, after the sign-in file was
rewritten, and its startup rows are not retained. A failure after 10:39 can
therefore be neither confirmed nor excluded from the logs. The issue stays
open.

Round 2 changed one thing in the product. The `codex_mcp` check still reports
`unknown`, and its next action now says that a `codex_apps` failure with 401
`token_revoked` means the provider rejected the stored sign-in, that every
newly launched Codex fails the same way until `codex login`, that a long-lived
Codex process that still fails afterwards needs a restart, and that the
official `codex doctor` checks the sign-in outside a session.

A live MCP probe through the app-server protocol was not built. It belongs to
the explicit self-test of #54. `doctor` stays read-only and local.

## Automated checks

| Check | Result |
| --- | --- |
| `cargo test --all-targets --all-features -- --test-threads=1` | 444 passed, 0 failed, 4 manual LIVE tests ignored |
| `cargo clippy --all-targets -- -D warnings` | Passed |
| `cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings` | Passed |
| `cargo clippy --all-targets --all-features --target x86_64-unknown-linux-gnu -- -D warnings` | Passed |
| `cargo fmt -- --check` | Passed |
| `git diff --check` | Passed |

The test harness ran serially. The four ignored tests are manual live tests and
are not runtime evidence. The cross-target checks compile and lint; they do not
run Windows or Linux tests.

## Side effects of the live checks

- Thirty-one task-owned Bridge sessions in isolated state directories
  (twenty-nine Agy, two Codex) and one direct Codex tab were closed explicitly.
  No session created by anyone else was touched. One `close-session` call
  failed with an iTerm2 AppleScript `Invalid index` error while an unrelated
  workflow was closing tabs; the retry closed the session.
- Agy's trust store gained eleven temporary workspace paths under the task's
  scratch directory: two approved by keypresses the task did not send, one by
  the reproduction paste, five by an Enter sent on purpose to the task's own
  test sessions, and three by the shared-consent response. They are harmless
  and can be removed by hand.
- The direct comparison created one Codex thread on the shared daemon. No
  credential, provider setting, or daemon was changed, and no session was
  resent.
- The evidence (request and result JSON, provider logs, screen captures, check
  logs) is in a temporary private scratch directory of the working session.

## Not verified

- Native Windows runtime for all three issues and for the Agy turn-failure
  report. The Windows Agy window of 45 seconds is kept for that reason; the
  Windows initial paste now uses the same two trust facts as macOS, checked
  only against the recorded Windows logs in the tests.
- Switching a managed Codex TUI to a second live agent thread. A Bridge-launched
  session is not connected to the shared background server, so the agent
  command centre offered no thread to switch to.
- The specific session in which the user saw the #56 error, and a live
  `token_revoked` reproduction. Reproducing it would require invalidating the
  user's sign-in.
- Agy on Ghostty. Round 2 used iTerm2 and Terminal.app.
- Agy turn failures other than the quota error. No other `agent executor
  error:` line exists in the logs of this Mac.
- None of the three issues is closed by this record.
