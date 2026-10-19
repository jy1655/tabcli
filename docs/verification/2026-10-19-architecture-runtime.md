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
At this checkpoint, the Claude-parent runtime and release gates remain pending.

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
