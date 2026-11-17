# Current Agy Boundary

- Agy has no first-party input path into a running interactive session and no per-turn
  accepted signal, so every paste into its TUI is a fallback that must be proven by Agy's
  own `--log-file`: a readiness gate before the paste and a `HandleUserInput` receipt
  after it. This holds for the native Windows initial prompt and for every follow-up on
  native Windows and macOS; the macOS initial prompt is a launch argument and never waits.
- Agy keeps its workspace-trust dialog over the composer until the exact workspace is in
  its own trust store (`trustedWorkspaces`), and it still runs an argument-delivered
  initial prompt behind the dialog. A paste onto the dialog is discarded and its Enter
  confirms the preselected "Yes, I trust this folder"; Agy then logs the trust reload
  (`Reloading system slash commands and skills` and three companion lines, 356 bytes) and
  no receipt. That is the "deferred skills reload" of issues #43 and #48: it trailed every
  lost paste because the paste caused it, and it never comes in a workspace trusted before
  launch (reproduced on demand 2026-10-01, Agy 1.2.14, macOS; more than 100 logs, no
  counterexample).
- Never paste without both trust facts: Agy's store lists the exact workspace, and this
  session's own `agy.log` shows the workspace customization load, a `hooks_manager.go`
  line from a goroutine other than the main one. The store is shared by every Agy
  process, so a dialog approved in one session leaves another session's dialog open; the
  log line is per process and appears at startup in a workspace trusted before launch or
  when the dialog is approved in that session. The macOS follow-up and the native Windows
  initial paste wait for both and fail `not_sent` at the deadline. Do not put a time
  window in their place, and do not answer the dialog without verified consent.
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
  macOS rule does not.
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

