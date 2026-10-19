# October 19 architecture review follow-up

This record follows the nine candidates in the 16:58 review of `b6c3e84`
(v0.4.20 plus PR #264). The owner requested review of all nine and implementation
and verification of the warranted changes. The initial cutoff below records local
work before release authorization. The later [runtime and release follow-up](2026-10-19-architecture-runtime.md)
records the owner's expanded scope and subsequent evidence.

All nine candidates have a disposition. Six received production refactoring, one
received paired tests, one received a documentation correction, and the common Launch
Judgment was declined. The implementation is on `refactor/architecture-review-165806`,
with product source frozen at `b7b02a1dcc09b68736e97b0a6e460c4cb666bd8c`.
Commit `d591a6e` updates only the relocated runtime-contract fixture.
Local integrated validation passed. The platform limits below remain explicit.

## Candidate dispositions

| # | Original candidate | Final scope and retained boundary |
| --- | --- | --- |
| 1 | Windows console helper protocol | Implemented. `console_control` owns request encoding, child decoding, payload and reply interpretation, and failure classification. The in-process test adapter uses the same decoder and provider resolver. Win32 effects and lifecycle-locked dispatch remain in the Windows adapter, with existing terminal operations and wire behavior. |
| 2 | macOS close intent and live-owner preparation | Documentation corrected. Handle retention and an actual close intent are different facts; Ghostty has no production intent writer. Adapter-owned preparation and ordered close observations remain. No new writer or close authority. |
| 3 | One launch Judgment | Declined. Binding observation, wrapper admission, final spawn admission, failed-start evidence, and never-ran evidence have different inputs, missing-record policies, and ordering. A common boolean or enum would hide those distinctions. |
| 4 | Typed session consent record | Implemented a permissive diagnostic observation with known state/applied-marker access. Raw diagnostic JSON and the separate strict authorization path are preserved. Provider trust lookup and applied-marker publication stay adapter-owned. |
| 5 | Shared bounded wait and Claim currency | Applied the existing `Clock` to legacy Codex submission polling and console input drain, replacing their injected-now/real-sleep mixture. Wait precedence and Claim predicates remain separate; Pi startup stays unchanged. |
| 6 | One Codex screen Judgment | Added three paired fixtures and retained both production recognizers. Initial empty-composer readiness and addressed pasted-draft acceptance answer different questions. |
| 7 | Doctor as a mapper | Moved its no-Snapshot partial observation into query observation. Partial errors, two separate lock scopes, probe ordering, and the later normal-path consent reread are preserved. |
| 8 | Request Judgment and Result envelope | Added typed request state and event identity access to Codex diagnostics, removing JSON reverse parsing. Ordinary lossy observation and the later strict event read after Snapshot release remain. No envelope framework. |
| 9 | Owner attestation test twin | Tests now execute the ordered production verifier with lazy process and TTY observers. macOS record decoding remains in the wrapper. The test-only twin and its unrelated Terminal.app/window assumptions were removed. |

## Why the broader merges were declined

`session::launch::wait_for_binding` is an unlocked observation, not permission to
spawn. `check_spawn_locked` checks the closed marker and deadline immediately before
spawn; `admit_wrapper` has neither of those responsibilities and permits a missing
legacy launch record. `failed_start_evidence_locked` instead requires the failed
launch's receipt and distinguishes an unresolved spawn. Windows' root-never-ran
check permits a missing receipt but rejects an unreadable one. Failed-start close
authority additionally depends on the terminal's facts and exact owner evidence.
These differences are deliberate contracts, not duplicate spellings of one judgment.

The trust wait can accept readiness before its deadline check, while input dispatch
must refuse expired authority. Initial Codex readiness can accept an exact empty
composer without a footer. A pasted draft needs an addressed boundary and separate
overlay checks; dialog words inside that draft can be ordinary user input. Neither
the wait frame nor the screen recognizer can be merged by collecting common words.

Consent observation is diagnostic evidence. It must preserve sparse, unknown, scalar,
and null JSON instead of silently treating them as an authorized schema. Recognizing
an applied marker does not grant permission: current authorization and its original
short-circuit order remain necessary. The marker declared by a provider and the
record that it was applied express different facts.

Doctor's normal consent reread observes a later point in its report construction.
Removing it would change behavior. Its fallback's two lock attempts also have distinct
failure scopes. Moving the observation must retain those scopes rather than prefetch
every record into one successful-looking report.

## Preserved execution contracts

- The helper's external argv, stdout/stderr, timeout units, and omitted/zero/positive
  send counts remain compatible. Provider defaults resolve after the session and
  surface are verified. Private file identity, ACL application, payload lifetime,
  and attachment-versus-payload read order remain intact.
- A proven refusal before writing can be `NotSent`; timeout after start, malformed
  success output, and failures after a write stay `Uncertain`. No text or Return is
  retried. Claim/session/expiry checks and each write share the lifecycle lock;
  screen reads and drain waits do not hold it.
- Typed request access does not change event-reader budgets or strict/lossy policy.
  The diagnostic releases the Snapshot before the existing strict provenance read.
- Owner verification observes the live owner before binding and complete identity,
  then optional surface TTY, TTY device, owner group, live parent shell, and shell
  group. A first failure prevents later observations. `surface_tty: None` remains
  supported, and legacy-permissive identity matching does not replace completeness.

## Validation

Baseline on native Windows at `b6c3e84`: 1,155 tests passed and five were ignored;
Clippy, formatting, diff checks, and macOS/Linux cross-target Clippy passed. A local
Codex 0.163.0 self-test passed initial prompt, initial result, follow-up, follow-up
result, and cleanup. These are baseline observations, not evidence for the refactor.

At `d591a6e38125972c2d948c62339b63317ad4e477`, the final serial suite passed **1,185 tests,
zero failed, five ignored**, across 16 suites. Its unit suite passed 1,008 tests
with one ignored; the remaining suites passed 177 with four ignored. The
following commands all exited zero on native Windows with Rust/Cargo 1.97.1:

```text
cargo test --all-targets --all-features -- --test-threads=1
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
git diff --check
cargo clippy --all-targets --all-features --target aarch64-apple-darwin -- -D warnings
cargo clippy --all-targets --all-features --target x86_64-unknown-linux-gnu -- -D warnings
```

The final real Codex check ended at **2026-10-19 19:30:33 KST**. After `cargo build --locked`,
the checkout's `target/debug/tabcli.exe` ran:

```text
self-test codex --workspace D:\Dev\agent-bridge --terminal windows-console --model gpt-6.1-sol --effort medium --timeout-secs 120 --json
```

Codex 0.163.0 passed ask, initial result, tell, follow-up result, and cleanup without
`--yolo`. Session `session-bYsylH` was closed by the self-test. The executable's
SHA-256 before and after was `c2fb91e9fd3300cbb5560a794f982a0a410bacb712fa7a251d6f7e5ed0340ce0`. This verifies one local initial/follow-up/close
round trip, not a failure-rate or performance claim.

Cross-target compilation does not run macOS observers, TTY operations, or surface
close. Native Windows CI has not been run for this local branch; repository policy
requires that separate runner evidence before claiming Windows behavior verified.
The live smoke covers Codex only. Authenticated Claude, Agy, and Pi checks were not
rerun on this branch. No push, PR, merge, release, or installation was performed.

Raw logs, first failures, reviewer results, the append-only decision trail and binary
identity are retained in the task directory
`C:/Users/user/AppData/Local/Temp/pstack-tabcli-architecture-165806`.
`final-summary.json`, `final-live-codex.json`, and `final-live-identity.json` identify
the final observations; baseline files and `final-tests-attempt1.log` remain separate.

## Corrections during verification

Two workstream file lists omitted callers of the changed private interfaces: six consent
test assertions and two Windows protocol tests. Their initial test builds failed; the
callers were migrated before integration. The new partial-observation test also tried
exclusive creation of an existing cancel record. Replacing it with an invalid record
through the atomic writer preserved the intended reread check, and all 13 observation
tests then passed. These were test migrations and fixture corrections, not runtime fixes.

The independent core review found one unused `parse_args` import left by the test move.
Its Clippy check failed until the import was removed; the same command then passed.
That review covered cards 1, 2, 3, 4, 7, 8, and 9 at `f57465c`, and found no other
introduced behavior regression. A separate review of `4e55996..679b5b3` passed the
Clock and paired-screen changes, checking observation order, sample count, errors,
dispatch locks, and the unchanged production screen predicates. Both code reviews
used `gpt-6.1-sol` at xhigh effort and standard speed.

The Clock worker stopped its long paste matrix before completion. That attempt was
not counted as a pass; the final serial suite covered it to completion.

The comment review removed 14 redundant comment lines in ten blocks. Its first four
structural suggestions were rejected because the named operation policy, canonical
directory check, explicit Snapshot scope, and independent lock expressions already
encoded those boundaries and had focused tests. One fresh `gpt-6-astra` high review
confirmed no remaining actionable findings and no further edits. Three comments about
Windows and PowerShell constraints remained; no executable code changed in this pass.

Several delegated sessions using the installed, unchanged tabcli 0.4.20 timed out
during initial input confirmation. No text or Return was resent to those sessions;
they were inspected and closed, and fresh assignments used shorter prompt files.
These are delivery limitations observed during orchestration, not passing live
checks of the refactored binary.

The first final suite passed all 1,008 unit tests but failed the integration test
`runtime_literals_survive_the_public_rename`. Its file-specific pins still named the
old locations. The five macOS verifier messages moved into shared ownership, while
the deleted test-only twin had contributed nine `native-session` occurrences to the
old source counter: macOS 6 became 1, and shared ownership 15 became 11 (15 - 9 + 5).
The console command and temporary-file prefix moved to the private client. Missing
process and submit-delay messages now have one constant each, used by both classifiers
and producers. The fixture was updated to pin those constants, classifier calls,
`Error: ` prefix handling, and both Windows producer references. It was not regenerated
from observed counts. The focused contract test then passed; no Rust source changed.
The initial failed log is retained separately from the restarted final suite.

The final decision-trail audit used `gpt-6.1-sol` at high effort and standard speed.
At its 19:33:25 KST evidence cutoff, it confirmed the final tests, static checks,
Codex round trip, cleanup, and runtime-contract fixture mapping. It found one log
pointer that cited the candidate review instead of the later user authorization.
The root verified the exact user reply and appended a correcting row, preserving
the original entry. The audit found no other false completion claim and retained
the platform and publication limits above.
