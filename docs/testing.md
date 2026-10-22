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

## Simulated time and the race hooks

`src/native/clock.rs` is the one clock seam of the native module: `Clock` (`now`, `sleep`),
the production `SystemClock`, and the test-only `FakeClock`, whose `sleep` advances the
simulated instant and whose shared handle lets a hook or a log reader move it from outside.
Tests inject it into the workspace trust wait, Agy's readiness gate and receipt wait, the Claude
messenger, the Codex queue's bounded send, legacy Codex submission polling, Windows console
input drain, the Origin observation
environment (which also carries the process attester), the macOS launch binding wait, and the
close scope. The close deadline stays an ambient scope by design, because record primitives and
the AppleScript diagnostics read it without a parameter; the scope's clock is a field of that
scope, set only by tests, and `close_now` reads it or the system clock. No production path
branches on `cfg(test)` for time.

The deterministic race hooks are the declared test adapter at the record seam that the
reproduction rule above needs: the fault budget, the sync log and sync failures, the publication
read log, the record replace retry hook, the close interruption points, enumeration injection,
the snapshot checkpoints, the Origin checkpoints and receipt counter, and the Windows termination
log. They stay thread-local test hooks beside the code they observe.

The console helper's in-process adapter uses the production argv decoder, session and
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

## Manual live verification

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
