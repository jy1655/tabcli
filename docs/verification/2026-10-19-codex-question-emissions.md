# Codex question-emission component verification

This covers the observation-only Codex component of #189. The broader question-response
and parent-wake work remains open. These checks use synthetic strings and identities,
without an authenticated provider session.

The original compiling regression is `8af1d36a85814c559e19443ad5f38917e80e754b`,
on base `7ce83b551458f2ce9fbc0ecfb07c261ebf46a37b`. A real Claim and Receipt, pending
frame, and `UserPromptSubmit` hook bind a synthetic rollout. The existing Observer polls
accepted user input and one async question item, without tool output or a terminal.
Before the feature implementation, `result --request` returned a null title where the
test expected the structured title. At candidate `58ad0337a0b56e2cfa70ec734189c98d98f71ba9`,
it exposes that title while the request stays pending, together with status attention and
inspect presence.

A second compiling regression, `763d6503db2be945bc44995d4fb5b1b56a06fdba`, covers the
terminal-verified/optional-flush boundary on that candidate. A test-only checkpoint runs
after the existing terminal integrity verification succeeds. Truncating the rollout there
left the public request pending instead of completed, with test exit 101. Correction
`3b5fa35a9c8ab5ca64e03046d4fb1bff987129a5` reuses that successful terminal checkpoint.
The regression passes for truncation and replacement, in ordinary polling and final drain,
through the real Observer and public result, status and inspect queries. The original
terminal verification and the separate nonterminal publication check remain in place.

Local native Windows focused checks passed after the correction: 44 transcript tests,
including 19 question tests, 46 query tests, 10 Codex lifecycle tests, 14 Codex diagnose
tests, and the runtime-literal policy test. All test harnesses ran serially. Component
coverage includes acceptance, optional attribution, terminal boundaries, id conflicts,
the full 64 KiB record limit, publication races, replay, before-rename and after-rename
failures, corrupt records, read-only queries, and excluded query contracts.

On this host, file-symlink creation returned Windows error 1314. The linked-record test
therefore used a directory junction to an empty directory. That branch proves refusal of
the junction path. It does not exercise reading or replacing an actual file symlink, and
the separate file target's unchanged bytes do not establish symlink-target protection.
The test reports which Windows branch ran. Actual file-symlink coverage requires a run
where file-symlink creation succeeds.

These component results do not establish full-suite, CI, cross-target or authenticated
live behavior. Lifecycle completion live evidence is separate in
[the lifecycle verification note](2026-10-18-windows-127-codex-lifecycle-hooks.md).
Legacy history, unprompted tool use, macOS runtime, other-provider questions, and every
addressed answer or resolution route are outside this component evidence.
