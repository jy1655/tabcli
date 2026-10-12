# Reopen policy source notes

## Source notes: 2026-10-12

These notes preserve details moved from source comments at `cc9df15` during issue #157.
They are prior source evidence, not new runtime verification.

### `src/native/provider/claude.rs:642` (before the sweep)

Official `claude --resume <uuid>` continues a conversation in a new interactive process.
The plan registers the new session's own private settings and cross-session name, so the
reopened process reports only into its own directory, exactly like a fresh launch. Policy
arguments (yolo, model, effort) are not part of the plan: the shared wrapper derives them
from the new manifest, which records only what the reopen request stated. That is the
whole of what Bridge controls: the flags it forwards. The effective policy of the
reopened process is Claude's own, decided from the resumed session, Claude's settings
files (re-read at launch), and its environment, none of which Bridge reads or overrides
(https://code.claude.com/docs/en/sessions#what-a-resumed-session-restores):
- Model: Bridge forwards `--model` only when the reopen request states it. Claude
  documents that a resumed session continues on its previous model unless a `--model`
  flag or an `ANTHROPIC_MODEL`-family environment variable picks one at launch, the model
  is retired or not in `availableModels`, or the provider uses deployment ids; the
  resolution order is Claude's model configuration, not the Bridge source manifest.
- Permission mode: Bridge forwards its bypass flag only when the reopen request passes
  `--yolo`. Claude documents that a terminal `--resume <session-id>` restores the saved
  mode except that a session which ended in bypassPermissions starts in the mode a new
  session would start in, and that a `permissions.defaultMode` from user, `--settings`,
  or managed settings takes effect there. Omitting `--yolo` therefore does not by itself
  establish that bypass is off: a configured bypass default still applies.
- Effort: the documentation lists no restored effort; only an explicit `--effort` is
  forwarded, and any effort default in Claude's own configuration applies otherwise.

Claude offers no exclusive hold on a conversation: resuming one session in two terminals
is permitted and interleaves both into one transcript
(https://code.claude.com/docs/en/sessions#branch-a-session). The registry gate is
therefore best-effort detection, never exclusion. It runs read-only before the source is
claimed, again immediately before the process is spawned, after launch once the new
process has registered under its managed `--name`, again immediately before the initial
prompt is sent, and again immediately before every `tell` to the resumed session. At each
of those points every other live holder of the same conversation is a detected conflict
that refuses the delivery. A foreign `claude --resume` can still register between two
checks and interleave into the transcript until the next boundary catches it; only a
provider-owned exclusive hold would close that window, and Claude does not offer one.

## Source notes: 2026-10-12 (Round 2)

These notes preserve prior source evidence omitted in the issue #157 round-1 sweep.
They are not new runtime verification.

Codex reopen prerequisites include the exclusive thread writer lock under
`~/.codex/thread-writer-locks` and the thread's rows in the queue store, whose accepted
inputs would execute in the reopened TUI. Resume-while-held behavior still needs a live
check. Preserved from `src/native/provider/codex.rs:92-95` at `cc9df15`.
