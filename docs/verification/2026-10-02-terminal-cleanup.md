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
zero-tab windows, and read errors (10 CLOSE, 7 WAIT, 7 VERIFY cases). VERIFY now
reports an identity error for a listed window whose TTY changed, acquired the killed-shell
suffix, or has no tabs; only a successful missing-window/app observation means absent.
All three TTY/empty-window cases failed the strengthened fixture before that correction.

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
explicit close still needs valid, exact ownership evidence.

The parent acceptance review exposed a separate false success before the first explicit
close. If the owner had already ended and no intent existed, repair marked the session
closed and consumed its last handle without adapter proof. Three independently executed
fixtures failed before correction, one each for Terminal.app, Warp and WezTerm. Repair
now retains those handles (including an interrupted claim). A complete, matching owner
and app identity can authorize a read-only absence check. Confirmed app death or a
successful exact-window absence check consumes the handle without any close or signal.
Present or unreadable surfaces retain it. Incomplete or foreign owner identities and reused
PIDs grant neither close nor absence authority. A failed `Pending` or `Spawning` launch
with a recorded owner follows these same rules: failure alone is no close intent.
Existing valid-intent retry remains supported.

The explicit failed-start recovery for iTerm2, Ghostty and Windows uses a different
creation identity. Its authority is the private handle bound before provider startup,
not the dead wrapper PID: iTerm2 targets its returned native session GUID; Ghostty
requires its returned terminal UUID inside the exact recorded tab and window; Windows
opens the recorded console root and checks its creation time and executable through
that retained process handle before attaching. Missing or mismatched target identity
does not authorize a replacement target. Ghostty closes only that terminal when a user
has added sibling splits, and checks disappearance after the close. This recovery
sends no signal to the dead or reused wrapper PID and does not interpret PID reuse as
surface absence. It requires a failed, non-`Spawned` launch and the same managed-session
binding; a live or uninspectable wrapper does not qualify. The ordinary live-owner
verification and the Terminal.app/Warp/WezTerm intent rules above are unchanged.

The native-ID basis is visible in iTerm2's installed `unique ID`/`guid` scripting
property and Ghostty v1.3.1 `ScriptTerminal.stableID` (the surface UUID, used for
`NSUniqueIDSpecifier`). Ghostty tab/window identifiers alone are not this authority:
its tab identifier is derived from a controller address, so every destructive operation
also requires the exact terminal UUID. This supersedes the earlier worker rationale
that merely cited released behavior or the former lack of a Ghostty presence probe;
the corrected Ghostty adapter exposes its read-only presence probe through the shared
macOS dispatcher. An absent terminal UUID proves absence; a UUID found outside the
recorded tab/window is a moved identity error and retains the handle.

## R3 lifecycle and creation corrections

The ownerless failed-start branch was separate from the recorded-owner fix above.
A deterministic production-path test reached Warp's destructive close without an owner,
prior intent or absence query. Ownerless Warp and WezTerm startup cleanup now only
queries the exact recorded surface: proven absence consumes the handle; present,
unreadable or foreign state preserves the handle and claim. The iTerm2 GUID, Ghostty
terminal UUID and retained Windows root exceptions remain explicit, as does a valid
prior close intent.

Ghostty's ordinary dead-owner repair was also reproduced reporting success and consuming
the last handle without observing its surface. Ghostty now retains its handle just as
Terminal.app, Warp and WezTerm do; explicit close must establish exact absence or valid
close authority. The failed-start native-ID exception remains separate from ordinary
repair. A regression checks both recorded-owner and ownerless Ghostty startup recovery.

WezTerm creation previously rejected the exact fresh spawn reply when an unrelated user
pane disappeared between snapshots. The regression failed before removing that unrelated
condition. The reply must still identify one fresh pane and tab in the requested window,
with a local tty and unchanged verified GUI incarnation; no delta is adopted or retried.

The three regressions failed before these corrections. Seven focused lifecycle/creation
tests passed afterwards, including prior-intent retry, recorded-owner rejection, exact
absence and Ghostty native-ID recovery. Whole-candidate checks and same-final independent
reviews remain separate evidence.

Four additional Ghostty regressions failed before correction: a moved terminal UUID
was misreported as absent before or during close, a long inherited PATH overflowed
the clean shell's canonical input line, and an unrelated old tab disappearing rejected
an otherwise exact fresh creation. Presence now scans the complete UUID inventory;
a moved UUID is an error that retains the original handle, never absence or adoption.
The shared presence dispatcher uses that same read-only observation. The initial input
sources a private session script carrying PATH and the existing launch command in the
same owned shell; quoted interpreter paths and the long PATH are executable test cases.
Ghostty fresh creation still requires the returned new tab/UUID and correct window,
but does not require unrelated user siblings to remain.

Warp's explicit new-window route (also used when no existing window is available)
uses its pinned official Launch Configuration schema independently of TabConfigs.
The existing-window tab route still requires TabConfigs; an uncertain creation never
triggers a second route. Actual Warp app behavior and configuration loading remain
NOT VERIFIED under this task's app-access restriction. The existing deadline-message
test flake was confined to which of two bounded deadline checks reported the failure;
both tests retain rejection and reprobe assertions while allowing either error text.

## App incarnation and failed startup

Window IDs and tty names can recur after Terminal restarts. A further pre-fix production
retry test reached the close adapter without any app-incarnation evidence and consumed
its handle. The native wrapper now records Terminal's PID and process start time from
the already-verified shell's ancestor chain before it starts the provider. OS process
identity and executable-path observations must find the system Terminal executable, and
the whole chain must still match on re-read. The owner record carries this identity into
the exact close intent. A live legacy owner can derive the app identity from its fully
verified ancestry and persist it before any close intent or signal. A dead or absent
owner never gains that authority.

A retry checks app birth before every close, wait, and second-close transaction. A reused
PID or unreadable OS result stops the retry; a proven dead app establishes absence
without contacting Terminal. Injected tests cover normal retry, dead app, reused PID,
unreadability, missing identity, changed ancestry and a restart between transactions.
The production ancestry walk was reproduced failing on the root-owned `login` in an
owned iTerm2 session: `PROC_PIDTBSDINFO` returned EPERM. It now reads parent and birth
through `sysctl KERN_PROC_PID`, retains the exact executable and lineage recheck, and
crosses that real root-owned ancestor successfully. This is OS process evidence; the
changed GUI behavior has not been observed in a live Terminal session under this
task's app-access restriction.

The initial command can fail before the native wrapper records its app identity. A
separate pre-fix rollback test showed that this path could call the adapter and remove
the binding without app proof. Failed-start rollback now unbinds only on a read-only proof of absence. A recorded app's
verified death proves absence without calling it. When no app incarnation was recorded,
the native process list must contain at most one Terminal instance, unchanged before
and after a successful window-absence query. This also permits safe cleanup of legacy
records; it never grants new close or signal authority. A still-live owner, incomplete
or foreign owner, changed or ambiguous app identity, present window or unreadable
evidence returns the startup/cleanup error and retains the handle. This
is a failed-start recovery limitation; the normal successful launch path does not use
rollback and is covered separately. If binding itself could not be persisted, the error
reports unverified cleanup; no durable retry record is claimed to exist.

A recorded app incarnation also needs an unambiguous scripting target while alive:
AppleScript addresses the application by name. A deterministic regression accepted a
second instance's absent-window reply before this check. The recorded instance must
now be the only Terminal process before and after a presence or close transaction.
A PID reuse, changed process list or unreadable list retains the handle. These separate
observations are not an atomic OS transaction; actual multi-instance GUI behavior is
not verified here.

The focused and full-candidate results are retained with the task's R2 review artifacts.
They do not establish physical window disappearance or identify the photographed window.

## Validation boundary

`macos_terminal_applescripts_compile_without_opening_a_tab` passed using `osacompile`
and installed application dictionaries. Compilation does not execute the scripts or
verify app-side behavior. Full integrated candidate checks and independent reviews are
tracked with the Warp implementation; these targeted results do not replace them.

The source fix has not been installed into the user's Bridge binary, and physical
window disappearance with the changed implementation remains unverified. The issue
stays open pending the remaining acceptance work.
