# Terminal.app cleanup failure and retry — 2026-10-02

Issue: [#64](https://github.com/jy1655/agent-bridge/issues/64).

This record covers deterministic source reproductions and compile-only validation. It
is not evidence that a particular live window was closed. No Terminal.app or Warp
surface was launched or controlled by these checks.

## Observed window and attribution limit

The user supplied screenshots of a blank residual Terminal window after Bridge recorded
cleanup as closed. Its visible title was `jy — Return Agent Bridge marker | agent-bridge
— 80x24`. The Inspector showed `No Selection`; its empty process list therefore does
not prove that no process existed. The Basic default profile had `Don't close the
window` selected, but the actual residual window's profile was not identified. No
window ID, tty or managed session ID was obtained from the screenshots.

The screenshots establish a residual window, not its exact session or root cause. The
source failures below were reproduced separately. The prior
[Terminal ownership verification](2026-10-01-terminal-proof.md) records a killed shell
whose Terminal tty gained U+0001; that prior observation grounds the supported suffix
case without attributing it to this new window.

## Reproduced script failures

Before the patch, deterministic AppleScript handler replays demonstrated these false
absence outcomes:

- `CLOSE_TAB_SCRIPT` returned `missing` for a supported killed-shell tty suffix, a
  changed tty, and failed window/tab reads.
- `WAIT_FOR_CLOSE_SCRIPT` returned `missing` while the exact window still existed but
  its tty changed or reading it failed.
- `VERIFY_TAB_SCRIPT` returned `missing` when the window query was denied.

The replay harness replaces application references with local mock handlers and
asserts that the replacement contains no Terminal application reference before running
it. The three original tests failed in seven cases before production scripts changed.

The fixed scripts retain exact window identity. Close recognizes the recorded tty or
that tty plus U+0001, propagates query/identity failures, and refuses changed or additional
tabs. An absent-app check does not start Terminal. Waiting for close checks whether the
exact window ID disappeared; tty loss, a zero-tab window, and a query timeout are not
proof of window absence. Expanded fixtures cover final identity change, stopped app,
zero-tab windows, and read errors (10 CLOSE, 7 WAIT, 4 VERIFY cases).

## Reproduced teardown/retry failure

`apple_terminal_close_retry_after_teardown_reaches_the_adapter` recreates the partial
transition after an authorized close killed its verified owner but the adapter failed
and restored the handle. Before the lifecycle patch, dead-owner repair consumed that
handle and marked the session closed. The retried adapter invocation count was zero
when the fixture required one.

A private `terminal.close-intent.json` now records the exact managed session, terminal
handle and fully attested owner after live verification and before any signal. A
matching intent whose owner PID is dead permits the same explicit close to finish
without signalling again. A live or reused PID grants no such permission. Interrupted
claims and another adapter failure retain the handle and intent. Successful close or
proven absence consumes them.

An additional pre-fix test exposed that a corrupt or mismatched intent still triggered
the old false-success repair. The corrected repair preserves pending cleanup whenever
an intent record exists; unreadable records fail. This preservation grants no authority:
explicit close still needs valid, exact ownership evidence. The unrelated case of an
owner that died without any explicit-close intent keeps its previous behavior.

The four `apple_terminal_close_` tests pass for successful retry (`Closed` and `Missing`),
repeated error, interrupted claim, live owner, corrupt/foreign/mismatched intent, and
unchanged no-intent repair. Worker-targeted adapter and close/repair tests also passed.

## Validation boundary

`macos_terminal_applescripts_compile_without_opening_a_tab` passed using `osacompile`
and installed application dictionaries. Compilation does not execute the scripts or
verify app-side behavior. Full integrated candidate checks and independent reviews are
tracked with the Warp implementation; these targeted results do not replace them.

The source fix has not been installed into the user's Bridge binary, and physical
window disappearance with the changed implementation remains unverified. The issue
stays open pending the remaining acceptance work.
