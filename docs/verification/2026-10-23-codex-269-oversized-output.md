# Codex oversized tool-output completion, 2026-10-23

Issue #269: a transcript record over 8 MiB suspended the bound observer, even when it
was an unrelated tool output and a valid terminal record followed. The deterministic
regression in `0622734` failed before the fix (zero published events, expected one).
It now passes with the bounded reducer and full UTF-8/JSON validation.

## Candidate and live observation

Implementation source: `971d2f70da69e7b225cc6389ddb713eadf858aaf`, including main's
#282. This report is a later documentation-only change. The debug candidate was built
by the full test command without replacing the installed executable. SHA-256:
`d9552fb51af9033ff062f67252c3af2267d564f2257ff5585bfcd4b43779531e`.

On macOS, Codex 0.163.0 in iTerm2 3.7.6 ran with explicit `--yolo`, the ordinary state
root and the existing trusted project workspace. A deterministic synthetic 1800 x 1800
RGB noise PNG (9,724,823 file bytes) was viewed exactly once, through `tools.view_image`
in Codex's `exec` tool. No shell command or other provider tool was called.

The actual bound rollout contained a **13,423,927-byte** newline-excluded
`response_item` / `custom_tool_call_output` at line 17. A `task_complete` for the bound
turn followed at line 23. The addressed result completed with `AB269_LARGE_OUTPUT_OK`
and provenance `codex_transcript_task_complete`, observed 27.762 s after receipt creation.
That elapsed time includes the whole request; it is not a reducer benchmark.

Owned session: `session-G9rntk`; request:
`request-1792735202385902000-67771-0`. Official turn:
`01a16758-6990-7bc0-9542-3266b0c05ac5`. The candidate's explicit close returned
`closed: true`, `ok: true`; the installed binary and plugins remained unchanged.
The bounded descendant scan timed out with no open descendants reported; no delegation
was requested or observed, and the named session's own close was confirmed.

## Verification limits

The full macOS serial suite passed **1,441 tests, zero failed, six ignored**; all-target,
all-feature Clippy passed on macOS, Linux GNU and Windows MSVC. The transcript tests
cover multi-image output, strict invalid-data rejection after elision, repeated or damaged
discriminators, relevant oversized records, bounded polling, replay, truncation, changed
claims and question publication. Formatting, diff and language lint passed.

Native Windows authenticated runtime, enormous user messages and arbitrary malformed
provider streams were not exercised live. Relevant records over 8 MiB still suspend;
records whose reduced structure exceeds 8 MiB also suspend. A long incomplete tool output
stays pending with bounded retention until its newline arrives. No bound claim falls back
to notify. The CI results for the final head are recorded on the PR separately.
