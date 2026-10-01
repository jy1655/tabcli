# Terminal.app ownership proof — 2026-10-01

Source baseline: commit `13e74f8` on branch
`fix/codex-follow-up-target-20261001`. The change recorded here fixes a
failure that was first seen while checking issue #58 and is described under
"Observations outside this change" in
[`2026-10-01-focus.md`](2026-10-01-focus.md).

This record describes a source change, its tests and its live checks. It is not
a release, a tag, or an installed update. The installed binary is still 0.0.8.

## The failure

The Terminal.app launch script proves which window it created by the tty of
the tab that `do script ""` returned: exactly one window must hold a tab with
that tty. On 2026-10-01 one launch (`session-y5Wpkl`) failed with `Agent Bridge
could not prove the newly created Terminal.app window`, and the window it had
just created stayed open without a managed session to own it.

Two windows reported the tty `/dev/ttys003` at that moment: the new window
(id 8335) and an earlier window (id 8332) whose shell had already ended.

## Cause

A tty name is not unique over time. Terminal.app 2.15 keeps a window whose
shell has ended in its window list, and what that window reports as its tty
depends on how it got there.

| Earlier window | Reported tty | Effect on a later launch |
| --- | --- | --- |
| Shell ended while a close confirmation was pending (ids 8332, 8338) | The old name, unchanged | Terminal had released the name. A later tab received it (`/dev/ttys003` went to window 8335, `/dev/ttys006` to an iTerm2 tab), so the proof matched two tabs |
| Shell killed, no confirmation pending (ids 8341, 8345) | The old name followed by the character U+0001 | The name was handed out again (`/dev/ttys014` went to window 8342), but the extra character prevented a match |
| Shell killed, no confirmation pending (id 8344) | The old name, unchanged | The name was handed out again within two seconds (`/dev/ttys014` went to an iTerm2 session of another task), so this state can collide as well |

The first state produced the recorded failure. The third state has the same
two ingredients, an unchanged name and a name that is handed out again, without
any pending confirmation. What decides between the second and the third state
was not established.

An earlier version of this record said that the name of window 8344 was not
handed out again while the window existed, because forty new ptys skipped it.
That reading was wrong. The name was held by the iTerm2 session described under
"A fault caused by this task" below.

## The change

The change is confined to the Terminal.app launch script.

- Before `do script ""` the script records the ids of the windows that already
  exist. The proof considers only windows that are not in that list, so an
  earlier window can never be matched, whatever tty it reports.
- A window that existed before the run is never returned, even when it is the
  only one that reports the tty.
- When the list cannot be read, nothing is excluded. That is the former rule.
- When Terminal is not running, the list is empty and nothing is asked of
  Terminal before the creation. The first Apple Event it receives stays
  `do script ""`. This also keeps the keyboard-window lookup of the #58 change
  from being the event that launches Terminal.
- The decision now lives in a handler without any `tell` block,
  `soleNewWindowWithTty`, so that it can be executed in a test.

One thing is unchanged by choice. When the proof still fails, the window that
was created is left open. After this change that happens only when the new
window is not found at all or when two new windows report the tty, and in
neither case is there a window that can be closed safely.

## Checks

The new test
`terminal_app_ownership_proof_ignores_windows_that_existed_before_the_launch`
takes the handler out of the shipped script and runs it with `osascript`. It
does not talk to Terminal.

| Case | Expected and observed |
| --- | --- |
| The recorded failure: windows 8335 and 8332 both report `/dev/ttys003`, and 8332 existed before | Window 8335 |
| The same two windows without the list of earlier windows | `could not prove the newly created Terminal.app window`, the recorded failure |
| No earlier windows, one other window with another tty | The window with the tty |
| The only window with the tty existed before | No proof |
| Two new windows report the tty | No proof |
| Both recorded windows existed before | No proof |

| Check | Result |
| --- | --- |
| `cargo test --all-targets --all-features -- --test-threads=1` | 446 passed, 0 failed, 4 manual LIVE tests ignored |
| `cargo clippy --all-targets -- -D warnings` | Passed |
| `cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings` | Passed |
| `cargo clippy --all-targets --all-features --target x86_64-unknown-linux-gnu -- -D warnings` | Passed |
| `cargo fmt -- --check` | Passed |
| `git diff --check` | Passed |

The test harness ran serially. The four ignored tests are manual live tests and
are not runtime evidence. The cross-target checks compile and lint; they do not
run Windows or Linux tests.

Live check on macOS 26.5.2 arm64, Terminal.app 2.15, Agy 1.2.14, isolated
state directory, with fourteen earlier entries in Terminal's window list, two
of them in the first state of the table above:

| Case | Evidence | Result |
| --- | --- | --- |
| `ask`, `tell`, `close-session` | `session-VuhKrC` | `ask` 9.5 seconds, `tell` 6.0 seconds. The window that was in front before the launch was in front again afterwards; the new window was in front for 699 to 823 ms |

With fourteen entries the scan of the window list takes longer, so the new
window keeps the keyboard longer than the 397 to 519 ms measured for #58 with
seven entries.

## Not verified

- The collision itself with the fixed build. The two reused names were in use
  by other sessions during this work, and producing the state again needs a
  window stuck on a close confirmation. The test replays the recorded case
  instead.
- A Terminal that is not running. Terminal could not be quit during this
  work. The claim about the first Apple Event rests on the script text.
- Native Windows, Ghostty and iTerm2 are not affected by this change.

## A fault caused by this task

Two iTerm2 launches of this task (`session-TeRuNV`, `session-kz8Yb9`) failed
after 30 seconds with `iTerm2 automation timed out`, and so did the launches
of another task on the same Mac from 19:52 KST on. `create tab` did not return,
while reading the tab list still worked. iTerm2 was waiting in its warning "A
session ended very soon after starting", which it shows as a modal dialog when
a session ends less than three seconds after it was created.

This task caused that dialog. A cleanup command for two experiment windows
ended processes by tty name in two passes, two seconds apart:

| Time (KST) | Event |
| --- | --- |
| about 19:51:50.9 | First pass (SIGHUP) ended the shells on `ttys014` and `ttys015`; both names were released |
| 19:51:52.869 | A new `login` process started on `ttys014`: an iTerm2 tab that the other task had just created (`session-q1eerT`) |
| about 19:51:53.0 | Second pass read the processes on `ttys014` again and sent SIGKILL. The new `login` was gone 0.1 seconds after it had started; its login record has no logout |
| 19:52:20.563 | `session-q1eerT` failed with `provider launch timed out before startup was confirmed` |

The kill itself and the dialog were not observed directly. The attribution
rests on the times above, the missing logout record, the launch log of
`session-q1eerT`, and a sample of iTerm2's main thread taken later.

It is the hazard this record describes, met from the other side: a tty name
does not identify a session over time. Processes must be ended by the process
ids that were read once, never by a name that is looked up again later.
