# Windows Reopen reservation refactor verification

Verified on native Windows on 2026-10-22 for architecture review card 3,
[PR #276](https://github.com/jy1655/tabcli/pull/276). This is a behavior-preserving
split of source reservation, launch-refusal settlement and typed refusal gates.

## Source and builds

The implementation branch is `refactor/windows-reopen-reservation`, based on
`602322f` after #275. Implementation commit `3d0e5ea` is followed by `25605f9`,
which limits the Windows observation import to Windows and test builds. The first
CI attempt found that import unused on Linux/MSRV; its Windows job was cancelled
by matrix fail-fast, not by a Windows test failure.

| Runtime build | Source | Executable SHA-256 |
| --- | --- | --- |
| Parent | `c6f2fd7`, whose entire tree equals `602322f` | `4faf319b5aed843f6a08f0394436cace737108241c37d3f5c3cfc61f3051bf7b` |
| Refactor | implementation working tree for `3d0e5ea` | `fc554b481880238307eea7d5328bce524e5d76ec024f557757eea59ecd9aa7f0` |

Both used `cargo build --release --locked`. After the refactor runtime binary was
built, the remaining changes were test-file placement, documentation/comments and
the import condition above; Windows production behavior did not change. The
installed executable remains the published v0.4.22, separate from both builds.

## Automated verification

- Native Windows serial suite: **1,200 passed, 0 failed, 5 ignored** with
  `cargo test --all-targets --all-features -- --test-threads=1`.
- Focused Reopen suite: **59 passed, 0 failed**. Existing deterministic launch-failure,
  interrupted-fence, conflict, marker-retention/release and source-history tests remain.
- Native all-target/all-feature Clippy and explicit Linux/Windows target Clippy passed
  with `-D warnings`; formatting and diff whitespace checks passed.
- Two added checks preserve exact gate wire spellings and typed error identity, and
  the existing auxiliary record decoder policy. Those decoders do not acquire the
  Snapshot timestamp-range rule or new schema acceptance policy.

The local suite preceded the final physical test-file relocation. The module names
and assertions stayed the same. The final source commit is covered separately by
[CI run 39767719830](https://github.com/jy1655/tabcli/actions/runs/39767719830);
all six jobs passed. Its Windows job independently reported **1,200 passed,
0 failed, 5 ignored** on final source `25605f9` (14 min 31 s including setup and checks).
Ignored authenticated tests are not live-runtime evidence.

## Authenticated parent/head comparison

Both builds ran the same sequence with Claude Code **2.1.308**, the ordinary state
root, a previously trusted workspace and `--terminal windows-console`, without
`--yolo` or changes to trust or permission settings:

1. `ask` returned exactly `CARD3_SOURCE_OK`; the source session was closed.
2. One `reopen` invocation had only the PATH entries containing `pwsh.exe` withheld.
   It failed before process/surface creation with `gate: launch-failed`. Its owner,
   provider-process and terminal records were absent, and the source marker was
   released. PATH was restored in `finally`; nothing persistent changed.
3. Reopening that source with the normal environment returned exactly
   `CARD3_REOPEN_OK`. A follow-up returned exactly `CARD3_FOLLOW_UP_OK`.
   Source, reopened result and follow-up reported the same provider conversation ID.
4. After closing the reopened session, another reopen of the source was refused
   with `gate: already-reopened` and no new session.
5. Every file in the closed source had the same SHA-256 before and after, excluding
   only `reopen.marker.json`. The pre-spawn failure also left those files unchanged.

Both sequences passed. The source, refused-launch and reopened sessions from each
build were all explicitly closed: six owned sessions total. No unrelated session
was closed. No uncertain delivery or input was retried.

The process PATH appended the npm directory to expose the Codex executable without
shadowing native Claude, as in the [baseline verification](2026-10-22-windows-0.4.22-and-consent.md).
This test's launch failure withheld PowerShell discovery only for that invocation.

## Limits and evidence

Live evidence covers successful resume/follow-up, a proven pre-spawn refusal and
safe retry, source-history preservation and duplicate-source refusal. PID reuse,
unknown process identity, concurrent holders and interrupted startup fences are
covered by deterministic component tests, not a claim of new live race reproduction.
This does not verify a new trust-dialog approval or #260's unmerged Claude native
completion path. Provider adapter implementations are unchanged.

Private JSON results, before/after source hashes and close receipts remain under
`.git/card3-live/`; the script is `.git/card3-live.ps1`. Local test and Clippy logs
remain under `.git/card3-*.log`. The report is kept on the Windows verification
branch, separate from the implementation branch and the installed release.
