# Codex lifecycle hooks (#127): Windows live verification

## Candidate and environment

Source: branch `feat/127-codex-lifecycle-hooks` at `8d8de83` (six commits on main `ec0190f`:
R1 `ad58fce`, R2 `cd1b951`, R1b `4982dd8`, R3 `523a528`, R4 `42907e5`, AGENTS.md `8d8de83`),
clean tree, no version bump (`--version` = `tabcli 0.4.16`). Observed on 2026-10-18 between
13:33 and 13:50 KST on native Windows 11 Home 10.0.26200 with Windows Terminal 1.24.12741.0
(dedicated `agent-bridge` window) and PowerShell 7.5.4; codex-cli 0.163.0 with a ChatGPT
account, default model `gpt-6-astra` at `xhigh`. The installed binary stayed the published
0.4.16 at `C:\Users\user\.cargo\bin\tabcli.exe`.

The candidate was this branch's `cargo build --release` (3,912,704 bytes, SHA-256
`94817cd9366186789dbaa37ae1bf2d411af5d8672cfe504e8b2e31641569feb0`), copied to
`C:\Users\user\.cargo\bin\tabcli-127.exe` and run by that absolute path for every command, so
that the hook trust hashes, which include the executable path, were the candidate's own. The
ordinary state root (`C:\Users\user\.agent-bridge\native-sessions`) and this checkout as the
workspace were used; every session was launched with `--terminal windows-console --yolo`,
unattended, and closed with `close-session --explicit` afterwards. Codex's `~/.codex/config.toml`
and `~/.codex/hooks.json` were edited only for A6, A7 and A8 and restored byte-for-byte after
each (hash compared).

## Automated checks

- Draft PR #250, CI run 39094261178 on `8d8de83`: rust (ubuntu, windows, macos), msrv, and
  both release builds passed, so the `cfg(windows)` tests of this branch have run on the Windows
  runner.
- Local serial gate on `8d8de83` (recorded in the session handover): `cargo test --all-targets
  --all-features -- --test-threads=1` 1041 passed / 0 failed / 5 ignored; clippy on the host and
  for `x86_64-unknown-linux-gnu`, `cargo fmt -- --check` and `git diff --check` passed.

## Hook trust (A15)

`tabcli-127 doctor --provider codex --probe --json` and `doctor session-PhyEdW --probe --json`
both reported `codex_hook_trust` as `available`: Codex's `hooks/list` returned
`C:\<session-flags>\config.toml:user_prompt_submit:0:0` with `currentHash`
`sha256:1701371f…` and `…:stop:0:0` with `sha256:e5915123…`, each `trusted` and equal to the
hash Bridge computed for `C:\Users\user\.cargo\bin\tabcli-127.exe` (`hash_matches: true`).
These differ from the R0 vectors because the executable path differs; the vectors themselves are
pinned by the unit tests.

## Live scenarios

Every record named below is in the session directory; times are KST from the records'
`*_unix_ms`. "Transcript" means the event's `provenance` is `codex_transcript_task_complete`
and the rollout holds a `task_complete` for the bound `turn_id`.

| # | Scenario | Session / claim | Observed | Outcome |
| --- | --- | --- | --- | --- |
| 1 | Short `ask` (injected) | `session-PhyEdW`, claim `6828-…-0` | `codex-launch-mode.json` `injected`, 0.163.0; binding 13:33:57.028 (thread `01a14d49-5b76…`, turn `…ad42804537e0`); Stop candidate 13:34:00.054 (`stop_hook_active: false`); notify record 13:34:00.129 (`input_messages_matched: true`); event 13:34:00.251 transcript; observer `settled` 13:34:00.296 at offset 141,632. The rollout's `user.text` of that turn carries the framed prompt and the marker; `task_complete` at 04:34:00.104Z. 28,052 ms. | passed |
| 2 | Title turn | same session | One `session_meta`, one binding, candidate thread equal to the binding's; no unmatched notify record. The temporary title thread left no Bridge record (its hooks are disabled upstream); raw notify payloads were not captured. | consistent |
| 3 | `tell` (queue, idle) | claim `27972-…-0` | New binding 13:34:35.061 (turn `…20ec5e238e10`), candidate 13:34:37.691, notify 13:34:37.775, event 13:34:37.788 transcript, observer settled 13:34:37.835. 9,664 ms. | passed |
| 4 | Steered TUI input during the turn | `session-PXJoVf`, claim `24064-…-0` | Task: numbers 1-400 as words. Binding 13:38:09.438; a second user message pasted through `native-console-control send` at 13:38:16.402 appears in the rollout with the same `turn_id` and no delegation header, and the binding file was not rewritten; one candidate 13:38:45.471 with the binding's turn; event 13:38:45.650 transcript, 402 lines ending `R5-A4-DONE`, `R5-A4-STEER-SEEN`. | passed |
| 5 | #127 reproduction: 25,463-character prompt, long answer | `session-dkpmIl`, claim `4148-…-0` | Framed prompt 25,655 chars / 25,966 units (pending-turn record). Binding 13:39:46.702, candidate 13:41:04.213, **no `codex-notify-*` record** (the notify spawn exceeds the command line), event 13:41:04.391 transcript with a 10,092-character answer (sum 35,555 characters), observer settled at offset 224,488. `doctor` reports `codex_notify_payload_budget` as `codex_notify_payload_not_required`. 110,184 ms, of which the console paste took most. | passed |
| 6 | User Stop hook blocks once | `session-4ch4ui`, claim `28792-…-0` | `~/.codex/hooks.json` held a Stop command hook (`a6hook.cmd`, prints `{"decision":"block","reason":"Append … R5-A6-CONTINUED …"}` once) trusted through `[hooks.state.'C:\Users\user\.codex\hooks.json:stop:0:0']` with the hash Codex's `hooks/list` reported. Rollout: assistant `R5-A6-OK` 13:48:40.479, `<hook_prompt hook_run_id="stop:0:C:\Users\user\.codex\hooks.json">` 13:48:41.006, assistant `R5-A6-CONTINUED` 13:48:43.486, `task_complete` 13:48:43.772 with that last message. Bridge: the surviving candidate (13:48:43.735) has `stop_hook_active: true` and the final text; one event 13:48:43.879 transcript with `R5-A6-CONTINUED`; the draft was never published. | passed |
| 7 | Hook-review picker | `session-SoxR6K` | Instead of a hash-drift build, the same user hook left untrusted (`hooks/list`: `trustStatus: modified`). The TUI opened "Hooks need review" (screen read through `native-console-control screen`); no console input was sent; at the 75 s deadline the launch failed with `Codex empty composer readiness was not observed; no initial console input was sent. The last screen contained Review hooks or Trust all and continue (hook-review picker); Bridge never answers this picker.`; `status.json` `failed`; close succeeded. | passed (variant) |
| 8 | `[features] hooks = false` | `session-dsmZcK` | `codex-launch-mode.json` `injected` (Bridge does not read Codex's feature flags); no binding, candidate or observer record; event provenance `codex_notify`, doctor `input_hook_observed: not_observed`, `terminal_outcome.kind: agent-turn-complete`. 27.5 s. Config restored identical. | passed |
| 9 | `settings codex-lifecycle-hooks off` | `session-vc69fM` | `codex-launch-mode.json` `not_injected`, `reason: settings_off`; no lifecycle records; event provenance `codex_notify`. 20.5 s. Setting restored to `on`. | passed |
| 10 | Esc during post-turn compaction | — | Needs a person at the keyboard and a compaction; not attempted. | not verified |
| 11 | Model error turn | `session-xDEyVf`, claim `5544-…-0` | `--model r5-nonexistent-model`. Binding 13:41:55.425; **no Stop candidate and no notify record**; rollout `task_complete` with `error.message` (HTTP 400 `invalid_request_error`) and `last_agent_message: null`; event 13:41:56.776 `failed` carrying that text, provenance transcript; session back to `ready`. | passed |
| 12 | Two `tell`s after idle: short, then 35,520 characters | `session-PhyEdW`, claims `26804-…-0` and request `…-10152-0` | Short: binding 13:35:15.045, event 13:35:19.672 transcript. Long: `codex queue` could not be started, `os error 206`, before anything was sent; request `unresolved`, no claim records, session `ready`, a later `tell` completed (row 13). The limit is on the queue's `--message` argument, not on notify: issue #251. | short passed; long not sent (#251) |
| 13 | Installed 0.4.16 `tell` into the candidate's session | `session-PhyEdW`, claim `25896-…-0` | `tabcli.exe tell` completed in 6.6 s with `R5-A13-TELL-OK`. The session's hooks and watcher are the candidate's: binding 13:42:25.043, candidate, notify, observer settled, event 13:42:28.460 with provenance transcript. The 0.4.16 reader printed the result without the provenance field. | works; mixed binaries remain unsupported |
| 14 | `self-test codex --terminal windows-console --yolo` | `session-pvvqbs` | All five steps passed in 37,143 ms; both events carry provenance transcript; cleanup closed the session. | passed |
| 15 | `doctor … --probe` | — | See "Hook trust". | passed |

Every bound claim's observer record ended `settled`; none was `suspended`. Every event's
`created_unix_ms` precedes its observer record's `observed_unix_ms` by 45-150 ms, which is the
order the publication code writes them in.

## Other observations

- Console-window fallback with a healthy Windows Terminal: with `wt.exe` removed from `PATH`,
  `ask` (`session-pjHOM2`) logged `surface=console-window; no Windows Terminal tab: Windows
  Terminal (wt.exe) was not found on an absolute PATH entry`, the root started, and the request
  completed in 21 s through the hook route. The 2026-10-18 12:19 failure (root left suspended)
  was therefore not reproduced; it happened while the Windows Terminal process, which also hosts
  the console window as the default terminal, was not responding.
- The notify record arrives 60-90 ms after the Stop candidate and before the transcript's
  `task_complete`; the watcher published 45-70 ms after the notify record. Notify remained
  evidence only: no bound claim was settled from it.

## Not verified here

- Row 10, a Bridge hash-drift build (row 7 used an untrusted user hook instead), and every macOS
  scenario of the R5 checklist (the Mac has 0.4.16 installed and Codex 0.163.0).
- Raw hook stdin and notify argv payloads were not captured; the records above are the
  adapter's own writes plus the rollout.
