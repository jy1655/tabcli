# Claude workspace trust dialog under bypass on macOS, 2026-11-10

Owner live check on one Mac with iTerm2. It does not cover Terminal.app, the terminals without a
guarded dialog answer, native Windows, Claude versions other than 2.1.321 in a live run, or a
workspace path that wraps in the dialog.

## Environment

- macOS 26.5.2, Apple Silicon, iTerm2, Claude Code 2.1.321, every launch with `--yolo`,
  `--model opus` and `--terminal iterm2`, in the ordinary state root.
- Published tabcli 0.4.38 for the reproduction; a debug build of branch
  `fix/claude-trust-dialog-under-bypass` on top of `758cc79` for the fix.

## What failed

Three `ask claude --yolo` requests in one workspace failed at 10:12-10:15 KST with `Claude
completion module readiness was not observed before the delivery deadline ... no message was
sent`. Their `workspace-consent.json` held `state: verified` (source `agy`) and `applied: null`.
Claude read each session's plugin directory only at 10:59, 44 minutes after launch; the day
before, Claude Code 2.1.319 sessions launched the same way in the same workspace read theirs
within 3 seconds. Whether 2.1.320 shows the dialog was not checked.

## Reproduction with 0.4.38

| workspace | result |
|---|---|
| new empty directory, no provider trust | `screen` showed the exact workspace trust dialog with `No, exit` selected; the request failed at the 45 s deadline with the readiness error |
| the failing workspace, by then trusted by Claude | completed in 11 s |

0.4.38 skipped the guarded answer for every macOS launch under bypass.

## Fixed build

| workspace | result |
|---|---|
| trusted by Codex only, verified consent | completed in 13 s; `applied: provider-workspace-dialog`; Claude's store then held the workspace |
| trusted by Claude | completed in 10 s; no dialog, no answer |
| new empty directory, no provider trust | failed at the 30 s deadline; the error added that Claude's own store records no trust for the workspace and that the session may be waiting at the dialog, with the `screen` command for that session |

Every session these checks created was closed.
