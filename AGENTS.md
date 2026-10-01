# Agent Bridge Project Instructions

## Product Direction

Agent Bridge exists to make first-party Codex, Claude, Agy, and Pi sessions simple to
launch, observe, continue, and close without replacing the capabilities those CLIs own.

## Provider-Native First

- Prefer each provider's official session messaging, follow-up input, result identity,
  lifecycle, permission, and close mechanisms whenever the installed version, platform,
  provider backend, and configuration support them.
- Do not introduce one shared transport merely because multiple providers currently need
  similar fallback behavior. Provider-specific integration work is intentional product
  work, not duplication to eliminate.
- Keep shared orchestration limited to provider-neutral session state and lifecycle
  contracts. Provider payload schemas and transport selection belong in
  `src/native/provider/<provider>.rs`.
- Every provider adapter must explicitly own its launch configuration, follow-up
  transport, completion identity, and any unsupported-platform fallback. Do not add a
  default adapter implementation that silently assigns the same behavior to all CLIs.

## Replaceable Fallbacks

- A fallback is allowed only where the first-party CLI lacks the required capability on
  the current platform, version, provider backend, or configuration.
- A fallback must preserve the semantics of the missing first-party feature, remain
  isolated inside that provider adapter, and document the upstream condition that makes
  it removable.
- When upstream support arrives, prefer deleting or replacing the provider fallback over
  extending the shared layer. The expected long-term direction is less bridge code as
  first-party support improves.
- Never claim a first-party path is active from version checks alone. Verify all relevant
  availability gates and keep the fallback explicit when the official feature is absent.

## Current Claude Boundary

- Claude Code v2.1.224+ provides official cross-session messaging on macOS and Linux,
  including WSL 2. Native Windows support is official from v2.1.234 and uses a
  per-session named pipe. Prefer the official path only when every runtime availability
  gate passes.
- Use the official `ListAgents` and `SendMessage` path on every supported Agent Bridge
  platform. Do not restore the removed native-Windows `--print --resume` supervisor or
  silently fall back to terminal input when official messaging is unavailable.
- Do not emulate Claude result correlation with a generic message hash. Identical valid
  responses can occur in separate turns; correlation must use Claude-owned identity or a
  provider-specific protocol with an explicit replacement boundary.
- Keep the follow-up payload out of the messenger model. The provider can stop or refuse
  a model response that carries arbitrary prompt text, which truncates the payload or
  skips `SendMessage` (observed 2026-09-18 with Claude Code 2.1.276-2.1.278). The
  messenger presents only a per-request reference, the `PreToolUse` guard supplies the
  addressed payload through the official `updatedInput`, and delivery counts only when
  the `PostToolUse` report of the executed input matches that payload.
- `SendMessage` is not permission-gated, even in `dontAsk` mode, and Claude discards the
  output of a hook that outlives its timeout and then runs the call unguarded. Neither a
  missing guard approval, the guard's own decision, nor an error result proves that
  nothing was sent: retry only when the guard approved nothing, Claude reported no
  executed call, and Claude itself reported every `SendMessage` call in the stream as
  blocked before it ran (denied with the guard's reason, or stopped by the provider).
- Give every request its own messenger files. The target can complete a delivered turn,
  and the next `tell` can start, while the previous sender is still settling.
- Never let a managed Claude session or a messenger inherit Claude Code session markers
  (`CLAUDE_CODE_CHILD_SESSION`, `CLAUDECODE`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`,
  `CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`, and the rest of the set in
  the Claude adapter). Agent Bridge is normally invoked from inside a Claude Code session,
  and a `claude` that inherits `CLAUDE_CODE_CHILD_SESSION` treats itself as a nested child:
  it never registers its cross-session inbox, so `ListAgents` cannot find it and delivery
  fails (issue #42, observed 2026-09-24 with Claude Code 2.1.281). The removal list is
  adapter-owned launch configuration; the shared launcher only applies it.

## Current Agy Boundary

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
- macOS has no reload window: trust, `CLI startup completed`, one quiet period, then the
  receipt. The Windows console gate still waits 45 s for the trust reload in a workspace
  trusted before launch, where it never comes; that wait is kept only because native
  Windows has not been re-verified since the cause was found. The reload logged right
  after `Starting new conversation` is that conversation's reload, never the trust reload.
  Agy logs `Full redraw completed` on the Windows console only, so the macOS rule does
  not require it.
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

## Change and Verification Rules

- For provider behavior changes, add or update tests in that provider's adapter and prove
  that unrelated provider adapters do not need modification.
- For concurrency or lifecycle changes, reproduce the race or partial transition with a
  deterministic test before patching it.
- Run `cargo test --all-targets --all-features -- --test-threads=1`,
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check`, and
  `git diff --check` before claiming completion. Use serial test-harness execution for
  local, CI, and release checks; retain concurrency created inside individual tests.
- Tests requiring authenticated CLIs and visible terminal surfaces remain manual live
  tests; do not represent ignored live tests as runtime verification.
- For Release workflow or release packaging changes, run the rehearsal
  (`gh workflow run release.yml --ref <branch> -f tag=<existing tag>`) and read its
  result before relying on a tag push. Only a tag push publishes, and a published release
  is immutable.
- In the Release workflow, verify tag properties through the GitHub API: the default
  tag-push checkout rewrites the local tag ref to the commit, and a checkout without
  persisted credentials cannot fetch again from this private repository. Only validation
  resolves the tag name; later jobs check out the validated commit SHA, and publication
  re-checks that the remote tag is still the validated tag object, because a tag can be
  moved while the run is in progress.
- Release scripts receive workflow expressions through `env`, never inline, so the policy
  tests can execute the exact scripts against fixtures.
