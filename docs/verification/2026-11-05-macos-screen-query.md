# Screen query on macOS, 2026-11-05

Owner live check of `tabcli screen` on one Mac with iTerm2 and a Codex session. It does not
cover Terminal.app, WezTerm, the refused terminals in a live run, or native Windows.

## Environment

- macOS 26.5.2 (25F84), Apple Silicon, iTerm2 3.7.7, codex-cli 0.166.0.
- Debug build of branch `feat/screen-read` at `2ca4c63`, SHA-256
  `136339372a24a50628d58a476f689101105e30d203ee4224f7e6485c9f12128b`, run by its absolute path
  with `AGENT_BRIDGE_NATIVE_STATE_DIR` pointing at an empty private directory.
- The Mac's screen was locked during the check (the frontmost application was `loginwindow`
  before, between and after the commands), so the check says nothing about focus changes on an
  unlocked desktop.

## Result

| step | command | result |
|---|---|---|
| 1 | `ask codex --terminal iterm2 --yolo --effort low --prompt-file ... --json` (prompt: reply with exactly `SCREEN_CHECK_7319 ready`) | `ok: true`, `request_state: completed`, Result `SCREEN_CHECK_7319 ready` |
| 2 | `screen <session>` | exit 0; 870 bytes on stdout holding the Codex TUI as shown: the framed prompt, the answer line `SCREEN_CHECK_7319 ready`, `Worked for 4s`, the empty composer and the model footer |
| 3 | `screen <session> --json` | `schema_version: 1`, `ok: true`, `terminal: "iterm2"`, `observed_unix_ms`, `screen` of 858 characters |
| 4 | file check | no file under the session directory was newer than the output of step 3 |
| 5 | `close-session <session> --explicit --json` | `closed: true` |
| 6 | `screen <session> --json` | exit 1, `ok: false`, error `session <id> is closed; its terminal surface is not read` |
| 7 | `screen session-doesnotexist --json` | exit 1, `ok: false`, error `no such Agent Bridge session` |

The read worked with the screen locked, like iTerm2 session launches do.

## Re-check after the post-read confirmation

A later fix (`c654849`) reads the binding again and repeats the ownership check after the read
before it returns the text. With a debug build of that commit, SHA-256
`1719e6bb07c10f44cd2e60440091daac147364e3dfc63ff85be83b40fe0302a2`, in another empty state
root and with the screen still locked: after `ask codex` completed with
`SCREEN_CHECK_8452 ready`, `screen <session> --json` returned `ok: true`, `terminal: "iterm2"`
and a 781-character screen containing that line in 0.73 s for the three terminal calls; after
`close-session` it exited 1 with the closed-session refusal.

## Re-check after the owner comparison

`5901627` makes the ownership check return the owner record it verified and requires the record
verified after the read to be identical to the one verified before it; `tell`, the launch dialog
and close call the same check and discard that value. With a debug build of that commit, SHA-256
`e860f6e9d16f074d0edf06b18dc49e0919700f2195e97e0c7c5af7774eaaf058`, in another empty state
root with the screen locked: `ask codex` completed with `SCREEN_CHECK_9087 ready`;
`screen <session> --json` returned `ok: true` and the Codex TUI text containing that line; a
following `tell` on the same session completed with `TELL_STILL_WORKS`; `close-session`
closed it. An owner replaced between the two checks was not produced live; the unit test
covers it.

## Not verified

- Terminal.app and WezTerm reads; the Ghostty and Warp refusals against a live session; native
  Windows (Windows Terminal tab and console window).
- The behaviour with iTerm2 not running: the scripts ask whether it is running and do not
  address it otherwise, and an application that quits between that question and the read can
  be started again by the scripting call. Neither was tried: quitting iTerm2 on a locked,
  shared Mac would have ended other sessions.
- Focus and selection on an unlocked desktop, a read during an active turn, a session whose
  launch failed with a live owner, and the 10-second deadline against a busy terminal.
- A Claude, Agy or Pi session: the read is a terminal operation and does not depend on the
  provider, but only a Codex session was read.
