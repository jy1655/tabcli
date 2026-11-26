# Current Agy Boundary

- Agy has no first-party input path into a running interactive session and no per-turn
  accepted signal, so every paste into its TUI is a fallback that must be proven by Agy's
  own `--log-file`: a readiness gate before the paste and a `HandleUserInput` receipt
  after it. This holds for the native Windows initial prompt and for every follow-up on
  every platform; the macOS and Linux initial prompt is a launch argument and never waits.
- Agy keeps its workspace-trust dialog over the composer until the exact workspace is in
  its own trust store (`trustedWorkspaces`), and it still runs an argument-delivered
  initial prompt behind the dialog. A paste onto the dialog is discarded and its Enter
  confirms the preselected "Yes, I trust this folder"; Agy then logs the trust reload
  (`Reloading system slash commands and skills` and three companion lines, 356 bytes) and
  no receipt. That is the "deferred skills reload" of issues #43 and #48: it trailed every
  lost paste because the paste caused it, and it never comes in a workspace trusted before
  launch (reproduced on demand 2026-10-01, Agy 1.2.14, macOS; more than 100 logs, no
  counterexample).
- With consent's authority finding the workspace consented (exact trust from another
  provider's store, or inherited by a verified worktree; owner's decisions, 2026-11-26) and
  the installed Agy, probed by this launch right before its command is built, at 1.5.17 or
  newer (its `--version` text must be the version alone), the launch hands Agy the exact
  workspace through its own `--settings` ("Settings for this session only, never saved"): a
  private `agy-session-trust.json` in the session directory holding
  `{"trustedWorkspaces":[<native key>]}`, then the applied marker `agy-settings-override`.
  The version is judged before any consent work, so an older Agy launches exactly as before.
  A file, not inline JSON, so that no quote reaches a Windows command shim; the argument is a
  path like `--log-file`. No key is sent and Agy's store is not written. For an ordinary folder
  that only another provider trusts this replaces the guarded answer (iTerm2, Terminal.app,
  Windows console), after which Agy saved the folder, and the launch refusal on a surface
  without a guarded key (Herdr, tmux, WezTerm) or whose screen cannot be read (Ghostty, Warp).
- Measured, all with Agy 1.5.17 on macOS (2026-11-26): standalone in an empty folder no store
  lists, inline or as a file, no dialog, the prompt answered, a `hooks_manager.go` line off
  the main goroutine and `settings.json` byte-identical afterwards; a settings change
  (`/thinking on`, then off) rewrote that store with its own key while the saved
  `trustedWorkspaces` stayed as it was, and returned it to its original bytes; through TAB on
  Herdr, a new worktree of this repository launched without a dialog, answered a follow-up and
  left the store unchanged. Not measured: Windows, Linux, the other surfaces, an exact-consent
  ordinary folder through TAB, and other settings writes. The 1.5.8 changelog already names
  `--settings`, but older versions were not measured with `trustedWorkspaces`, and an Agy
  without the option fails on it. Delete the file when Agy offers a trust option of its own.
- Never paste without both trust facts: Agy was told to trust the exact workspace (its
  store lists it, or this session's record says its launch configured the setting and
  consent's authority finds the workspace consented now), and this session's own `agy.log`
  shows the workspace customization load, a `hooks_manager.go` line from a goroutine other
  than the main one. The store is shared by every Agy process, so a dialog approved in one
  session leaves another session's dialog open; the log line is per process and appears at
  startup in a workspace trusted before launch or when the dialog is approved in that
  session. The setting is TAB's configuration, not evidence, and the marker is history, not
  authority: an Agy that ignores the setting logs no such line, and a revoked or withdrawn
  consent refuses the next paste. Every paste waits for both, the Windows follow-up included
  (it skips only the startup readiness wait), and fails `not_sent` at the deadline. Do not put
  a time window in their place, and do not answer the dialog without verified exact consent
  that no launch option applied.
- After a downgrade to 0.4.49 a session launched with the setting keeps working where that
  binary does not need the setting: a gated paste (every macOS and Linux follow-up) passes
  only when Agy's own store lists the workspace and the log has the load line, and a
  Windows follow-up is not gated by 0.4.49 at all. A gated paste that relied on the setting
  alone is refused as not sent; close that session and start a new one, which shows Agy's
  dialog under 0.4.49.
- A log without that line withholds the paste but does not prove an open dialog: the log
  can be missing, or cut before the line, in a session that has none. Report what was
  read (the session's trust is unverified) and keep the recovery conditional on what the
  managed terminal shows: approve a dialog that is there, otherwise replace the session.
- Neither platform has a reload window: trust, `CLI startup completed`, one quiet period,
  then the receipt. The Windows console waited 45 s for the trust reload until native
  Windows was verified again (2026-10-01, Agy 1.2.10 and 1.2.14): in a workspace trusted
  before launch that reload never comes, and a paste right after the quiet period was
  received. Do not put a window back. The reload logged right after `Starting new
  conversation` is that conversation's reload, never the trust reload. Agy logs `Full
  redraw completed` on the Windows console only, so the Windows rule requires it and the
  macOS rule does not. A Windows Herdr terminal keeps the Windows rule unmeasured: if Agy does
  not log that line there, the paste is withheld and reported not sent, never sent blind.
- The trust dialog matcher accepts only the exact dialog, alone or followed by a footer
  that names an Agy model family (`Gemini `, `Claude `, `GPT-`): the footer shows the
  saved model, which is not always a Gemini one, and appears about a second after the
  dialog.
- A missing receipt is delivery-uncertain, never a second paste; the claim is kept and the
  reason lands in `status.error`. Delete the gate and the receipt when Agy exposes an
  input API or an accepted-turn signal; the transcript result monitor is unaffected.
- A turn Agy gives up on leaves no result in the transcript, only an `agent executor
  error:` line in `agy.log` (2026-10-01: `RESOURCE_EXHAUSTED (code 429): Individual quota
  reached`, which made every request wait out its timeout). The result monitor records it
  as a failed request for the pending claim only when the log itself shows the newest turn
  to be the pending one: a pasted turn behind the receipt that carries its marker, or the
  first, argument-delivered turn when the only `USER_INPUT` step of Agy's full transcript
  carries the marker. The error must follow that turn's `Forwarding user message` line.
  A result Agy has already written is recorded first; once the failure is recorded the
  claim is released and a later result of that turn is not accepted. Only the quota
  error has been observed, so do not assume the same for an error Agy might recover
  from. Do not bind a failure by the transcript alone: a newer claim's input can reach
  the transcript before its log lines are written. Replace this with a per-turn failure
  signal when Agy provides one.

