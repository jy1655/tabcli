# Claude pre-approval trust dialog and the version note on macOS, 2026-11-10

Owner live check on one Mac with iTerm2 and Claude Code 2.1.322, every launch with `--yolo`,
`--model opus` and `--terminal iterm2`. It does not cover the pre-approval dialog without
`--yolo` in a live run, Terminal.app, native Windows, the singular wording, another settings
file, or a version note for a provider other than Claude.

## The dialog variant

With published tabcli 0.4.39, `ask claude --yolo` failed twice at its deadline in a workspace
that Codex trusted and Claude did not, with `applied: null`. `screen` showed the workspace trust
dialog with one more paragraph between the access sentence and `Security guide`:

```text
⚠ This folder pre-approves 2 tool permissions in .claude/settings.local.json:
  Read(<path>) and
Read(<path>)
These will apply without asking. Only proceed if you trust this configuration.
```

The workspace's `.claude/settings.local.json` pre-approves those two permissions. A workspace
without such a file, trusted by Codex only, completed in 14 s with 0.4.39.

With a debug build of branch `feat/claude-preapproval-dialog-and-version-note`, the same
workspace completed in 13 s with `applied: provider-workspace-dialog`.

## The version note

| state root | result |
|---|---|
| ordinary root, whose newest completed Claude session ran 2.1.322 | a launch in a directory nobody trusts failed at its 30 s deadline with the error text of 0.4.39 and no note |
| private root holding the manifest and events of one completed Claude Code 2.1.319 session | the same launch failed at 30 s; the error read `failed to deliver the initial prompt to claude session <id> (this session runs claude 2.1.322 (Claude Code); the last claude session with a completed request on this state root, <id>, ran 2.1.319 (Claude Code): if the provider changed its startup, ...): Claude completion module readiness was not observed ...`; the recorded `session_error` had no note |

The ordinary root held about 4,700 session directories; the failure arrived at the deadline
(29,957 ms) as before.

Both tables are from the first commit of the branch. After review the dialog matcher was
narrowed to exactly two listed permissions, and the version scan was bounded and made to end
without a note on anything it cannot read. The narrowed matcher was run once against the
captured text of the screen above (answer under bypass, none without); no launch was repeated,
because the workspace had become trusted by Claude. The bounded scan was covered by tests, and
run once from a debug test binary against the ordinary root (4,713 session directories): it
answered in 1.1 s, inside the 5 s the caller waits.

With the final build of the branch, the private-root launch was repeated once and failed at
30 s with the same note, and a launch in a workspace Claude trusts completed in 10 s.

Every session these checks created was closed, and the private root was removed.
