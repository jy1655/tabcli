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

## Not verified

- Terminal.app and WezTerm reads; the Ghostty and Warp refusals against a live session; native
  Windows (Windows Terminal tab and console window).
- That the command never starts a terminal application that is not running. It was not tried:
  quitting iTerm2 on a locked, shared Mac would have ended other sessions.
- Focus and selection on an unlocked desktop, a read during an active turn, a session whose
  launch failed with a live owner, and the 10-second deadline against a busy terminal.
- A Claude, Agy or Pi session: the read is a terminal operation and does not depend on the
  provider, but only a Codex session was read.
