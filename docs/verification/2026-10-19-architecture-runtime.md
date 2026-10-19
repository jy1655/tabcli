# October 19 architecture runtime verification

This follow-up extends the [architecture review](2026-10-19-architecture-review-follow-up.md)
after the owner requested actual runtime checks, release after verification, and a Claude
parent using Codex and other providers. Earlier verification cutoffs remain historical.

## Candidate and environment

The initial matrix used `4e07cb3a0b7d7e47c52c4cc132a2f1df6b9f7b3e`, built with
`cargo build --locked`. Its debug executable still reports 0.4.20; the 0.4.21 release
metadata was prepared separately. Product source has not changed since `b7b02a1`.
Before and after every matrix case, the executable SHA-256 was
`c2fb91e9fd3300cbb5560a794f982a0a410bacb712fa7a251d6f7e5ed0340ce0`.

The host was native Windows x86-64, OS build 26200, PowerShell 7.5.4, and Windows
Terminal 1.24.12741.0. The caller was Codex using PowerShell. Cases used the ordinary
Bridge state root and the existing workspace consent. No `--yolo` override or global
trust/permission change was used in this matrix. Claude, Agy, and Pi used their configured
models; Codex explicitly used `gpt-6.1-sol` with medium effort.

## Provider and surface matrix

Each successful case ran the checkout executable's `self-test PROVIDER --workspace
D:\Dev\agent-bridge --terminal windows-console --timeout-secs 120 --json`. Codex added
`--model gpt-6.1-sol --effort medium`. Five steps had to pass: initial delivery, exact
initial result, follow-up delivery, exact follow-up result with a distinct Request and
event, and cleanup. The recorded events were independently reread and matched to the
unique marker. `inspect`, `inspect --timeline`, `doctor`, and `result --list` all exited
zero after close; the closed record and absence of its owner process were checked.

| Provider | Version | Windows Terminal tab | Native console window |
| --- | --- | --- | --- |
| Codex | 0.163.0 | `session-pvHfsR`: all five passed | `session-z67QMN`: all five passed |
| Claude | 2.1.303 | `session-tw8doJ`: all five passed | `session-PUwD3f`: all five passed |
| Agy | 1.4.2 | `session-d4js0w`: all five passed | `session-aKpT0t`: approval timeout; fresh `session-V7zFwr`: all five passed |
| Pi | 1.3.0 | `session-2XP4wN`: all five passed | `session-l5Fdh8`: all five passed |

These runs ended between 20:40 and 20:49 KST. To force the fallback, the runner removed
only PATH entries containing `wt.exe` from its own process, restoring PATH afterward.
Actual `surface=console-window` launch records and absence of the tab-host record
confirmed the fallback; tab cases had their host record and tab launch evidence.
The requested terminal option alone was not treated as proof of the surface used.

Codex's events identify `codex_transcript_task_complete`. The other providers retained
their existing completion contracts. These short round trips do not verify every
failure mode, cancellation, large-prompt limit, or latency distribution.

## Preserved failed attempts

The first Agy console case received the complete pending marker and then requested
approval for `git grep "agent-bridge-agy-turn"`, despite the self-test saying that no
tool, command, or file was needed. Its log, actual console text, and full transcript
agree. Doctor reported `agy_tool_confirmation_observed`; the result wait timed out
after 120 seconds, no follow-up was sent, and cleanup passed. Bridge did not approve
the tool or retry input. A separate fresh session passed without a bypass override.
This reproduces the model-selected tool approval limitation already described in
[Testing](../testing.md); the fresh pass does not remove that limitation or establish
a failure rate.

The first Claude parent, `session-1cmgkA`, paused before reading the harness because
the fixture was outside the workspace. It was closed without running the harness.
The fixture was then copied into the ignored `target/runtime-follow-up` directory.
The second parent, `session-gbvwn4`, read the harness but Claude Code's auto-mode
classifier denied its PowerShell invocation with `Create Unsafe Agents` before the
script started. Its raw tool result and missing output records support that refusal.
The owner was asked for one exact command rule confined to that managed session.
No rule was added while that decision was pending. Both parent attempts were closed;
authorization would apply to one fresh verification parent. The Claude-parent runtime
and release gates remain pending.

## CI candidate archive and independent review

The Windows archive from PR CI run `39248419782`, source `c93ab8c`, passed its checksum
and exact member checks. LICENSE and THIRD_PARTY_NOTICES.md differed from their Git
blobs only by the Windows checkout's complete LF-to-CRLF conversion. The initial
byte-exact text comparison rejected that conversion; the follow-up check established
that it was the only difference. Archive and executable bytes were not changed.

The extracted executable reported `tabcli 0.4.21`, with SHA-256
`64219395f13e4646e2f7e52a98b79526115d84fd059e8e0b0795c45300b2d428` before and after
the runs. Codex (`session-KFdow8`), Claude (`session-qtqYNi`), Agy (`session-J3U1Mm`),
and Pi (`session-yQXPg6`) each passed all five real self-test steps on Windows Terminal
tabs without `--yolo`. All four sessions closed. These are PR candidate runs, not
verification of a main-push or published release asset.

The independent `gpt-6.1-sol` high audit used a 21:00:48 KST evidence cutoff. It
confirmed all 43 copied raw-record hashes, the original eight successful round trips,
the separate Agy approval timeout, and the 1,185/0/5 local result. Its first attempt
stopped at private-session read permissions without a completed review; the fresh
review used workspace-scoped copies. Neither reviewer received escalation.

It identified two checks missing from the unexecuted nested harness: strict failure
on unsuccessful or unknown owner inspection, and an assertion that each child's
recorded origin matches the actual managed Claude parent. Both checks were added;
the script also checks provider conversation continuity and successful close identity.
Its syntax passed, but execution is still unverified. Caller marker names alone do
not prove child environment isolation. The prepared Claude-child case adds a
process-local `CLAUDE_CODE_CHILD_SESSION=1` sentinel and would require a successful
official follow-up, then restore the caller environment. That case has not run.

Rehearsal run `39246849668` completed successfully with `workflow_dispatch` against
old tag `v0.4.20`; it did not publish and does not validate this candidate's source.
PR #265 remains a draft. Main-push CI, tag creation, release publication, and local
installation are not complete at this checkpoint.

## Claude parent verification after the permission-mode correction

At 23:36:47 KST the owner corrected the intended default to explicit `--yolo`. The
previous exact-command permission proposal was superseded and never applied. A new
Claude parent, `session-zUjtY5`, and every new child received `--yolo`; no global
permission or trust setting changed. This corrects the verification workflow, not
Bridge's policy: `--yolo` still requires an explicit launch option and is never inherited.

Two fixture defects were retained as failed attempts. The first invocation read
`AGENT_BRIDGE_SESSION_ID` instead of the actual `AGENT_BRIDGE_NATIVE_SESSION_ID`
and stopped before launching a child. After correcting that and the directory variable,
four children returned their exact initial answer and closed, but the script incorrectly
required `event_id` in `ask` output. That field belongs to `result --request`; no follow-up
was sent in that attempt. The separate v2 harness queries each addressed result and
preserves the original outputs. No product source changed for either correction.

The actual Claude shell executed v2, using the same verified PR Windows executable
above. From 23:44 to 23:46 KST all four complete round trips passed:

| Child | Session | Initial / context-dependent follow-up | Close |
| --- | --- | --- | --- |
| codex | `session-FUGBuW` | Exact provider token + `42` / same token + `43` | Closed, owner absent |
| agy | `session-ZjMx3O` | Exact provider token + `42` / same token + `43` | Closed, owner absent |
| pi | `session-huuWUU` | Exact provider token + `42` / same token + `43` | Closed, owner absent |
| claude | `session-uFUQQH` | Exact provider token + `42` / same token + `43` | Closed, owner absent |

The follow-up did not repeat the token or the first answer. Each pair had distinct
Request and event identities and retained one nonempty provider conversation identity.
Independent reads of raw event files and addressed result queries matched both answers.
Every child's recorded origin was the actual managed Claude parent, and its configured
`yolo` value was true. Close and inspect exited zero with a closed state, a boolean false
owner observation, no owner inspection error, and no recovery requirement.

The caller record contained the six checked Claude marker names, without recording
their values. The Claude-child case also explicitly set `CLAUDE_CODE_CHILD_SESSION=1`
in the calling process and restored it afterward; its official follow-up succeeded.
This exercises the known inherited-child-marker failure path. It is not a complete dump
or independent inspection of every child's environment.

Private evidence includes `nested-independent-verification.json`, the actual Claude
provider transcript, and `target/runtime-follow-up/nested-v2` with the exact ask, tell,
addressed result, close and inspect outputs. An initial independent-verifier invocation
ran before the fourth row existed and refused the incomplete data; the completed four-row
verification passed. Earlier failed fixtures and approval refusals remain separate.
The parent reported the completed run and then closed with no open started-from children.
A fresh independent `gpt-6.1-sol` high review at a 23:48:48 KST evidence cutoff
verified all 145 export hashes and confirmed the raw nested round trips and cleanup.
It required a final log checkpoint for the parent close and PR readiness, retained
the bounded environment coverage, and kept release conditions separate. That log
checkpoint was appended; the reviewer session was also closed.
The requested nested runtime gate is now satisfied. Publication still requires the
latest PR checks, exact main-push CI, annotated tag, and release asset verification.

## Evidence and remaining scope

At release preparation commit `83e9f32`, the local serial suite passed 1,185 tests,
zero failed, and five ignored. All-feature Clippy, formatting, and diff checks also
passed. This used a separate Cargo target directory so the running verification
sessions retained their original executable. No `src/` or `tests/` file changed
between the matrix source and this release-preparation commit.

Private evidence is retained under
`C:/Users/user/AppData/Local/Temp/pstack-tabcli-architecture-165806/runtime-follow-up`.
`run-case.ps1`, the per-case JSON, `matrix-summary.json`, and `summarize-matrix.py`
preserve the matrix and exact result checks. Complete diagnostics, transcripts, and
console text remain private; only reviewed excerpts appear here.

Authenticated macOS runtime, its Automation prompts and live TTY ownership, and cold
Windows Terminal startup are not verified by these runs. Linux managed sessions are
unsupported. CI, release packaging, and published-asset checks are separate from this
local matrix and must not be inferred from it.

## Main CI completion-record recovery, October 20

PR #265 merged as `3ce788207018816be4f164420ba5ba0cbcfd1de2`, with exactly the
reviewed tree and all six PR checks successful in run `39273026679`. Its exact-main
push CI was run `39275683672`. Both platform archives passed checksum, member, license
and notices checks, and the Windows executable reported 0.4.21.

The Windows job's downloaded log reports 1,185 passed, zero failed and five ignored,
then reaches orphan-process cleanup at 2026-10-19 15:30:36 UTC. Afterward GitHub kept
reporting that job and run as in progress, with a null conclusion; the job's steps became
empty and the job-list endpoint intermittently returned HTTP 502. Cancellation replied
that the run was already completed; force-cancellation replied that it was not in
progress (HTTP 409); both CLI and REST rerun requests said it was already running
(HTTP 403). These responses do not establish a successful CI conclusion.

The original log, API observations and rejected recovery requests are preserved privately.
This documentation checkpoint starts a fresh PR and main-push validation path, with
product source, tests, dependencies, version and workflows unchanged. No tag or release
was created from the inconsistent run. Final publication still requires all six checks
and a successful CI conclusion for the exact main commit that receives the tag.
