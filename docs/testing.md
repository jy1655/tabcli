# Testing

Run the automated checks below for a code change; use `self-test` when you need to exercise
a provider in a visible terminal surface. Report the two separately.

## Automated checks

The ordinary unit tests require Node.js (`node` on `PATH`) to run the real Pi extension
through its test harness. Without Node.js, that test fails with an explicit prerequisite error.
On native Windows, the unit tests also require PowerShell 7 (`pwsh.exe`) on an absolute `PATH`
entry, as managed sessions do.

Run the required local checks from the repository root:

```sh
cargo test --all-targets --all-features -- --test-threads=1
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
git diff --check
```

Run the test harness with one test thread, as required by AGENTS.md. This policy was adopted
after deadline-sensitive fake-Codex tests failed under the parallel harness in
[issue #46](https://github.com/jy1655/tabcli/issues/46); concurrency inside individual
tests remains enabled.
For a lifecycle or concurrency fix, reproduce the partial transition or race deterministically
before changing the implementation.

Claude's native completion tests replay an exact addressed start, an unmarked bound interim
and a marked completion with a different official turn id. They assert a retained claim before
the final, an unchanged original binding, one result and the original Bridge wait identity.
The continuation tests cover malformed or missing candidates, unrelated outcomes, close and
claim replacement, timeout retention and interruption between identity selection and publication.
`tests/claude_completion.rs` executes the real JS module through native-hook, including helper
retry, an oversized unbound completion and read-only candidate inspection. These fixtures do
not prove provider parentage; authenticated background ask/tell evidence is recorded separately.

### Claude inbound module coexistence

Other Claude modules may consume or rewrite inbound input. The deterministic coexistence
fixtures model possible target events; they do not run `session.receive`, reproduce Ruflo,
or establish Claude's hook order or the sender-visible outcome of consumption. No production
delivery or completion policy changes are implied by these tests.

| Fixture | Evidence checked |
| --- | --- |
| `native_messenger_success_without_target_turn_keeps_the_claim` | A controlled messenger passes the real guard, executed-input receipt and stream confirmation. With no target turn, the waiter reports a timeout and a second claim is refused. Both preserve every session file, including the claim, pending input, status and receipt, and publish no result. |
| `coexistence` in `tests/claude_completion.rs` | The real completion module and native-hook reader reject a marked final without a start, a changed body with its marker intact, a removed peer envelope, combined input and an unrelated turn. Each checkpoint preserves the claim, pending record, status and receipt; `result` stays pending and changes no session records. |
| The same `coexistence` fixture's exact-input control | After those rejections, the unchanged addressed frame and matching marked final publish exactly one result with the original Bridge correlation id. No input is resent. |
| `late_old_frames_and_markers_cannot_settle_a_successor_claim` | A real successor claim and pending record survive the old request's start/final and an unbound answer copying the successor marker. The successor's own exact frame still completes, and the first result remains unchanged. |

Run these checks from the repository root with Node.js available:

```sh
cargo test --all-features native_messenger_success_without_target_turn_keeps_the_claim -- --test-threads=1
cargo test --all-features late_old_frames_and_markers_cannot_settle_a_successor_claim -- --test-threads=1
cargo test --all-features --test claude_completion -- --test-threads=1
```

The Node harness invokes the candidate's native-hook binary; it is not a standalone mock
of Bridge's lifecycle. Its synthetic starts use the same peer framing and protected-tag
quoting as the existing large-wire fixture. A successful sender and a target completion
are separate evidence surfaces. A missing target turn alone does not diagnose consumed input.
The marked-continuation fallback retains its existing limits: these negative fixtures have
no valid bound-turn candidate, and do not prove parentage when such a candidate exists.

#### Manual diagnostic-plugin recipe (NOT-VERIFIED)

Live coexistence remains **NOT-VERIFIED** until an owner runs a controlled probe against
the exact installed Claude version and records both sender and receiver evidence. This is
an optional diagnostic recipe, not an automated gate or permission to change user plugins.

1. Use a task-owned verification directory, a candidate binary with its SHA-256 recorded,
   and a fresh managed session for each of pass-through, consume and rewrite modes. Record
   provider version, platform, permission mode and every configured plugin path and hash.
   Keep existing plugins enabled and provider trust and global settings unchanged.
2. Implement only a temporary diagnostic plugin using the installed Claude module types.
   First verify the actual `session.receive` input/result schema. If unavailable, record
   `NOT-AVAILABLE`; do not invent a handler signature or substitute terminal input. Scope
   its action to a unique harmless probe nonce; other input must pass through unchanged.
   Log observed hook entry, invocation of the next handler, and actual return values in
   a bounded private log (for example, 64 KiB total), marking truncation or failure unknown.
3. Add that plugin only to the task-owned launch, preserving Bridge's completion plugin.
   Do not infer runtime ordering from argument order. Instrument observed `turn.start` and
   `turn.complete` identities/text for the nonce, and the receive hook's actual trace.
   A configured consume/rewrite mode is not evidence that the hook ran or took effect.
4. Deliver one probe per fresh session, with one absolute timeout (for example, 60 seconds).
   Record the sender's actual `ListAgents`/`SendMessage` stream, guard approval and executed
   input receipt separately from target events. A sender success, timeout or absent target
   event proves neither consumption nor hook order. Never repeat an uncertain delivery.
5. At the deadline, read the original request with `result` and `inspect`, then inspect its
   owned surface. Preserve the pending record, immutable binding if present, diagnostic,
   receipt and result evidence. Do not edit these records, force completion, send another
   `tell` while claimed, or use a caller timeout to retire the claim. Close only the probe's
   session when appropriate; use another fresh session for the next mode.
6. Record configured mode/order separately from observed receive outcome, observed
   start/completion identities, sender result and Bridge request state. Missing or truncated
   observations stay `NOT-VERIFIED`. Preserve eligible evidence under the workspace policy;
   remove only the task-owned diagnostic plugin after its sessions are closed.

## Offline diagnostic formatter

The offline diagnostic formatter has a focused standard-library suite:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p test_share_diagnostics.py -v
```

It covers all supported public query shapes, free-text and misleading typed-field canaries,
unknown categories, exact version forms, missing/null/invalid distinctions, bounded reads and
outputs, malformed JSON, duplicate keys, non-finite numbers, Unicode/control/bidi content,
unchanged input objects/bytes, and the stdin/stdout CLI. These are formatter checks and supply
no authenticated provider or terminal-runtime evidence.

## Language lint

Public documentation, source comments and default plugin metadata use English. Run from the
repository root with Python 3.9 or newer:

```sh
python3 scripts/check_language.py
```

CI runs this check on Linux, macOS and Windows. It inspects tracked and non-ignored new text
files, reports `file:line` for non-ASCII letters or combining accents, and exits nonzero on
findings or an invalid policy. Punctuation, arrows and emoji are allowed. JSON and JSON-compatible
YAML quoted strings are also decoded so Unicode escapes cannot hide a translated display name.
This is a character check, not a classifier for foreign words written entirely in ASCII.

`.language-lint.json` contains the exceptions and a reason for each: the Korean README,
unchanged upstream notices, two historical Korean verification reports, and Unicode test/fixture
paths. Unicode examples in production files have exact-line exceptions, so the rest of each file
is checked. The policy itself contains those Unicode examples. An unused exception fails the
check and must be removed or updated with its fixture.

Use a translation path or an exact-line exception for a legitimate new example; keep public
plugin metadata under the default rule. Review exception changes as part of the diff.
The checker tests run with the existing Python suite:

```sh
python3.12 -m unittest discover -s scripts -p 'test_*.py' -v
```

## Simulated time and the race hooks

`src/native/clock.rs` is the one clock seam of the native module: `Clock` (`now`, `sleep`),
the production `SystemClock`, and the test-only `FakeClock`, whose `sleep` advances the
simulated instant and whose shared handle lets a hook or a log reader move it from outside.
Tests inject it into the workspace trust wait, Agy's readiness gate and receipt wait, the Claude
messenger, the Codex queue's bounded send, legacy Codex submission polling, Windows console
input drain, read-only Request polling in `wait` and `result --wait`, the Status scan and its
Session observation and snapshot retries, the Origin observation environment (which also carries
the process attester), the macOS launch binding wait, and the
close scope. The close deadline stays an ambient scope by design, because record primitives and
the AppleScript diagnostics read it without a parameter; the scope's clock is a field of that
scope, set only by tests, and `close_now` reads it or the system clock. No production path
branches on `cfg(test)` for time.

Status shares one clock with its snapshot retries and Origin observations. Its budget tests
keep real Readers and locks, advance the fake clock through the existing observation hooks,
and check which sessions remain listed or receive a busy or budget-exhausted reason. They
preserve the first snapshot attempt even after its deadline, and retain an already-read
session when its Origin observation consumes the remaining scan budget. Ordinary queries
keep their system clock, publication policy and 250 ms snapshot retry window.

The deterministic race hooks are the declared test adapter at the record seam that the
reproduction rule above needs: the fault budget, the sync log and sync failures, the publication
read log, the record replace retry hook, the close interruption points, enumeration injection,
the snapshot checkpoints, the Origin checkpoints and receipt counter, and the Windows termination
log. They stay thread-local test hooks beside the code they observe.

The console client owns an explicitly supplied `HelperProcess`; the installed adapter never
selects test behavior. Tests can keep two independent clients in the same scope. The console
helper's in-process adapter uses the production argv decoder, session and
provider resolution, payload decoder, input dispatcher, and reply interpretation. Its fake process
and Win32 effects do not prove real attachment or OS behavior. The owner fixtures likewise call
the production ordered verifier with lazy process and TTY observers; they do not run the macOS
observers on Windows. The [October 19 follow-up](verification/2026-10-19-architecture-review-follow-up.md)
records the nine candidate dispositions and the separate verification limits.

An I/O context carried on `Store` and `Reader` was proposed as the next step and declined on
2026-10-17. Its propagation cost is the reason: every `open_unchecked` call would carry it, and
at e2b3b7d the pattern `(Reader|Store)::open_unchecked(` matches 778 call sites in `src/`
including tests (341 `Reader`, 437 `Store`). Its benefit over the hooks above is limited to test
hygiene, since those hooks already reproduce the transitions the rule requires. A future review
should not re-propose it without a new reason.

Also check platform-gated code with the two cross-target Clippy commands in
[Contributing](../CONTRIBUTING.md). Installing `rust-std` for each target is enough for those
checks. Cross-target checks do not run tests on the target platform.

On macOS, `macos_terminal_applescripts_compile_without_opening_a_tab` compiles the iTerm2,
Terminal.app, and Ghostty scripts with the installed application dictionaries. Install Ghostty
and iTerm2 in their normal Applications locations before running the suite. This test compiles
scripts without opening a surface; it does not verify live Automation consent or delivery.

Check which test was ignored before interpreting the count:

- `tests/native_live.rs` contains four provider smoke tests requiring authenticated Codex,
  Claude, Agy, or Pi and a visible supported terminal surface. They are manual live tests.
- The WezTerm adapter has an ignored live test that starts, binds, and ends its own real surface.
- The self-test unit tests contain an ignored subprocess fixture used by a deadline test.
  It is not an authenticated provider test and should not be counted as one.

An ignored live test has not exercised the provider. Use `self-test` for an authenticated round
trip rather than enabling every ignored test, which also runs the subprocess fixture.

## What CI runs

[The CI workflow](../.github/workflows/ci.yml) runs on pushes and pull requests to the main branch:

| Job | Platform | Checks |
| --- | --- | --- |
| Minimum Rust | Ubuntu, Rust 1.97.1 | Formatting, all-feature Clippy, serial all-feature tests |
| Stable Rust | Ubuntu, macOS, Windows | Formatting, all-feature Clippy, serial all-feature tests |
| Release candidate | Apple Silicon macOS, x86-64 Windows | Locked build and package checks |

The formatting command in CI is `cargo fmt --all -- --check`; Clippy uses
`cargo clippy --all-targets --all-features -- -D warnings`. Tests use the serial command above.
The macOS stable job installs Ghostty and iTerm2 for the AppleScript compile test.
The release-candidate job packages the executable and license, checks the archive checksum,
extracts it, and checks the extracted binary's version. This is not an authenticated live test.
Linux CI checks portable code; Linux managed sessions remain unsupported.

### Windows CI performance observations

Windows CI and Release tests run the same serial Cargo command through
`scripts/ci_test_timing.py`. It streams combined stdout/stderr, preserves a failed Cargo
exit status, and uploads `windows-test-timing-ci-windows-latest-<attempt>` or
`windows-test-timing-release-windows-latest-<attempt>` for 30 days, including after a failed test step.
No artifact is available if the test step never started; cancellation or runner loss can
also prevent a final record or upload. Historical release rehearsals whose checked-out tag
predates the script keep the ordinary serial command.
Capture setup failures stop the step before Cargo starts. A capture failure after a successful
Cargo run fails the step; if Cargo itself fails, its failure status takes precedence.

Completed captures contain `output.log`, `timings.json`, and `summary.md`. Before starting Cargo,
the recorder saves a `running` record with no result; it atomically replaces it after exit.
An interrupted capture can retain that initial record and the flushed partial log. Neither
proves completion. Files live under `RUNNER_TEMP`, outside the checkout, and the release
publisher downloads only its three named binary/integration artifacts.

The JSON records the actual checkout SHA, tree and dirty status, command, Rust/Cargo versions,
runner OS/architecture, CPU and image information, Cargo's reported compilation duration, each
libtest suite's stable source identity, reported duration and counts, and test output intervals. Environment capture uses
an explicit allowlist, not an environment dump. The workflow SHA is retained separately:
it can differ from the source checked out by a release rehearsal.

The command duration includes compilation. Start-to-result and completion-to-completion output
intervals include buffering, fixtures, cleanup and harness overhead; they are diagnostic clues, not profiler
measurement or provider latency. Output whose format the parser does not recognize remains
in the raw log. Overlong or non-UTF-8 lines are not parsed and are counted diagnostically.
A Cargo `Finished` or libtest result line whose duration or count is malformed or not
representable is counted as unparsed; such a result line leaves its suite open. An unexpected
parser fault stops parsing and is recorded. Neither stops Cargo or changes its exit status,
the raw log or live output; the comparison treats both as incomplete observations.
A suite whose parsed test count differs from libtest's result is incomplete for comparison.
No permissions, durability, test selection, retries or concurrency are changed.

Download two artifacts and compare them with Python 3.12 or newer:

```sh
python3 -I scripts/ci_test_timing.py compare before/timings.json after/timings.json
```

The comparison names different or missing runner/toolchain conditions, changed suite counts,
dirty checkouts, and failed or incomplete runs. Matching metadata still does not control hosted
load or caches. There is no automatic wall-clock threshold: use repeated comparable samples
before choosing one. To collect the same evidence locally, run
`python3 -I scripts/ci_test_timing.py run --output-dir <new-directory-outside-the-checkout>`.

The SID regression guard and these observations address the repeated subprocess cost measured
in [the Windows CI investigation](verification/2026-10-24-windows-test-runtime.md). Native
identity and DACL readback tests remain required; performance observations do not replace them.

`tests/windows_security_policy.rs` parses the production security module on every host.
Its exact dependency allowlist rejects new subprocess APIs, helper indirections, modules,
attributes and uninspected macros; adversarial mutations include the old `helper_command`
route. Only the exact external `#[cfg(test)] mod tests;` declaration is exempt, so the
Windows `whoami` test oracle remains available. Unused exceptions fail too. Update the
allowlist only after reviewing the dependency and rerunning native Windows tests.
This is a source boundary, not a process counter or a Rust resolver: it trusts the
allowlisted items, does not resolve methods or associated items through `Self` or a local
type alias, and does not detect macro shadowing in ancestor modules. It checks only the
`security` declaration in its immediate parent: `#[path]` or `cfg` redirection of an ancestor
module, and re-exports that route callers to another definition, are outside it.

## Manual live verification

For controlled comparisons of saved self-test runs, see
[Compare saved self-test experiments](self-test-comparison.md). The offline comparator
keeps failed attempts and cleanup outcomes, separates confirmed skipped steps from measured
times, and requires explicit experiment identities and distinct runs across baselines.

Self-test opens a real surface and makes real model calls. Authenticate the selected CLI,
review the workspace, and make sure the terminal application answers before starting.
For macOS system approvals, see [macOS permissions](macos-permissions.md).

To test this checkout, run the following from the repository root after reviewing the workspace:

```sh
cargo run --locked -- self-test codex --workspace . --json
```

If the managed terminal shows a workspace-trust dialog, review and answer it yourself.
Self-test does not answer it. A failed command does not mean that no prompt reached the provider.
Record failed and timed-out attempts. Before another run, check the terminal application and
the report's close outcome for any session created. Do not resend a prompt whose delivery is
uncertain. A timeout before a surface exists is not a pass; a later pass does not establish
the cause of that timeout.

The exact-marker prompt says that no tool, command, or file is needed. Agy nevertheless
requested a `RunCommand` approval with the earlier wording in one default-mode run
(Agy 1.3.0, 2026-10-07). Unit fixtures verify the read-only `doctor` observation and its
inclusion in a self-test result timeout reason. They do not prove that the new wording
prevents approval prompts. For live verification, record the installed Agy version and
compare default-mode runs with the old and new wording. Bridge does not answer approvals.

For Pi, an initial or follow-up result timeout reads `doctor SESSION --probe --json`
within five seconds before cleanup and includes the session's provider credential readiness
in the step reason. Pi's `ready` status proves configuration only, not successful authentication.
The diagnostic does not prove a prompt refusal or change the pending request or claim.
Unit fixtures cover ready, missing credentials, unknown provider, missing command, timeout,
malformed output, model resolution, and timeout reason enrichment; they do not verify a
managed Pi turn or the TUI's refusal events.

An unconfirmed self-test close is `not_verified`, including a timeout. The cleanup reason names
the session and carries its recorded surface error. A Ghostty failed-launch handle remains available
to explicit close only when its handoff precedes close and the launch still owns the session.
A close after the launch deadline but before the handoff leaves the residual surface named in the
closed session's error, with no recoverable handle; Bridge does not close it. That recorded residual
surface is also preserved as typed status evidence and exposed by `inspect` as
`residual_surface: "unverified"`; changing the error text cannot clear it. It makes cleanup
`not_verified` even when a handle-less close succeeded. A successful recovery
close can pass cleanup, but does not turn the failed launch into a passed round trip. Unit tests
reproduce both orderings, failed discovery, and failed cleanup with a fake Ghostty runner; they do
not verify locked-screen behavior or the cause of an automation timeout.

Hold tests exercise four deterministic interleavings through the production writer: after the
first admission, after claim creation, before delivery begins, and after it begins. Additional
fixtures cover initial-delivery exclusion, failed settlement I/O, close ordering, idempotency,
malformed records, read-only refusal, partial observations, close preservation, and pruning.
They do not exercise authenticated provider delivery.

Delivery tests exercise the Claim interface for sent, not-sent, and uncertain outcomes, both
before and after provider completion or a successor claim. They verify that late reports cannot
rewrite another turn, not-sent rollback preserves its reason, and uncertain input remains claimed
even if a diagnostic write fails. These deterministic transitions do not prove live transport
delivery. Residual-surface tests cover legacy records, diagnostic changes, interrupted tombstone
amendments, handle persistence failures, and positive versus refused adapter closes.

Turn completion fixtures use the provider Report interface; Claim has no separate completion
path. Reopen's deterministic tests live with its module and cover source reservation, provenance
failure, concurrent attempts, late refusals, and surviving provider processes. Agy's transcript
test also compares read-only Result evidence with incremental publication before and after a
truncated row's full body arrives. These tests require no authenticated provider.

Warp fake-runner tests cover control-binding write failure with confirmed cleanup (no handle)
and unconfirmed cleanup (retained handle and failed status), close refusal during pending
creation, and late handoff into an already closed session with a warning that survives repeated
close. Recovery through Warp's exact close after this write failure remains unimplemented:
the saved surface ids do not replace the missing control binding. These tests establish neither
a live record-write failure nor live Warp support (#63, #87).

For release verification, run the packaged candidate's executable instead and record its path
and SHA-256.

Replace `codex` with `claude`, `agy`, or `pi` for that provider. Use `--terminal` to select a
supported terminal explicitly; consult [Terminals](terminals.md) for limits. Warp needs a
reachable authorized Control endpoint, which its Stable and Preview builds cannot publish: the
Scripting setting is behind Warp's internal-only `WarpControlCli` flag (see
[requirements](terminals.md#requirements-and-first-run-prompts)). Its control API cannot submit
terminal input, so follow-ups without a provider-native input path are unsupported.

Self-test launches a session, verifies the exact marker result of the initial request, sends
one follow-up, verifies a distinct request and result event, and closes its owned session.
Close is confirmed through a read-only inspection. It never resends an uncertain prompt or
deletes records. It follows `ask`'s workspace-consent rules: it reuses verified consent for the
exact workspace through the provider's own approval, and without that consent it approves no
workspace trust itself. An explicit `--yolo` only forwards the provider's own bypass option.
Every invoked command is bounded.
Only a fully verified round trip and close exit successfully.

The default is the ordinary state root, whose path is reported. Closed records remain there.
`--isolated` uses a private directory: ordinary-root settings and consent records do not apply,
and that directory remains until you remove it. `--timeout-secs` is a per-command budget,
defaulting to 120 seconds, not a budget for the whole run; close uses separate bounded calls.
Record any model, effort, or bypass override you add to the command.

## Record the evidence

Create a dated Markdown record under `docs/verification/`. Include:

- The tested commit, Bridge version, binary SHA-256, build method, OS, and architecture.
- Provider CLI and terminal versions, caller environment, and relevant consent settings.
- Commands and explicit overrides, including whether ordinary or isolated records were used.
- Automated check results, with ignored tests and checks not run stated separately.
- Each live provider/terminal combination, session identifiers, step outcomes, and close outcome.
- Failed and timed-out attempts, observations supporting the diagnosis, and what remains unknown.
- Unexercised combinations and paths, including the providers' default permission modes if unused.

Keep full evidence privately and publish only reviewed, redacted excerpts. Do not upload session
directories, transcripts, tokens, or complete diagnostics; see [Security](../SECURITY.md).

The [macOS 0.1.2 record](verification/2026-10-06-macos-0.1.2.md) is an example: it records
Codex round trips in WezTerm, Terminal.app, iTerm2, and Ghostty, plus Claude in WezTerm.
It explicitly leaves Warp, Agy, Pi, native Windows runtime behavior, and default approval modes
unverified for that candidate. Those runs used bypass overrides, unlike the example above.
The record also preserves surface-creation timeouts instead of counting them as passes.

A macOS run cannot verify native Windows console behavior. A native Windows run cannot verify
macOS scripting, surface ownership, or system approvals. Use an appropriate host for each;
cross-compilation and CI unit tests cannot substitute for authenticated live runs.
Release notes must name the combinations exercised and link the dated evidence, then state
unexercised combinations as not verified. Earlier records establish only their recorded
versions and conditions, not a fresh runtime pass for the current candidate.
