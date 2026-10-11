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
  adapter-owned launch configuration; the shared module only applies it. Provider
  commands (managed launches, version and doctor probes, messengers and the Codex queue)
  keep their own adapter's removal list. Non-provider commands on macOS, including
  terminal automation that can start an application and helpers, drop the union of the
  adapters' caller-session markers and Bridge's state directory, session directory,
  session id and executable variables. A child that needs one receives it explicitly.
  Non-provider environment cleanup on native Windows is pending issue #111. An already
  contaminated terminal application must be restarted.

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

## Current Pi Boundary

- Pi v1.0.4 exposes `ctx.abort()` to an interactive extension, but no verified official
  endpoint addresses that running interactive session from outside. Cancel therefore
  reaches the official API through a Pi-owned record and polling fallback. Replace the
  polling and acceptance record when Pi provides that addressed interruption endpoint.
- A captured context resolves the runtime at call time; it does not identify a turn.
  Start the 200 ms timer at `agent_start`, invalidate its run before clearing it at
  `agent_end`, `agent_settled`, shutdown or a new run, and require both the current Claim
  and its correlated prompt before aborting. A callback of an old run stops nothing.
  The interval is not a response-time guarantee.
- Call abort at most once for a Claim. The call and the atomic publication of
  `pi-cancel-accepted.json` cannot be one operation; a failed publication never permits
  another call. Acceptance is not completion. Only the same Claim's accepted call,
  the last assistant's abort outcome at `agent_end`, and matching cancel intent permit
  a cancelled Result at settlement. Recognize `stopReason: "aborted"`, or `"error"`
  with exactly `errorMessage: "This operation was aborted"` (tool abort observed on
  Pi 1.0.4, 2026-10-11). Do not classify other errors by an abort substring. Direct Esc
  stays an ordinary failure and identifies no actor. Exit before reporting keeps the
  ordinary recovery semantics.
- The installed extension proves cancel capability with schema 2 startup readiness.
  The adapter carries that evidence to the next Claim and clears its acceptance record
  at pending-turn installation or rollback. Never upgrade a schema 1 session by version
  inference. Other modules do not read Pi's acceptance record.
- Retry and compaction can settle without a new aborted `agent_end`; record what Pi
  reports, not what the cancel intended. Pi returns queued messages to the editor on
  abort. Bridge uses no Pi queue and deletes no editor input. Tell the user to check
  the surface before another `tell`, whose paste can combine with restored input.

## Current Codex Boundary

- Codex gives no identity of a turn before its first `notify`. The adapter appends a turn
  marker to the prompt and accepts an `agent-turn-complete` payload when the assistant
  message ends with the marker, or when the last element of `input-messages` is the
  prompt Bridge sent: it begins with Bridge's delegation header and ends with the marker.
  The thread of the first accepted payload becomes the session's thread, which every
  follow-up addresses, so a wrong first payload is a wrong result and a wrong thread.
- The marker alone does not identify the turn. Codex CLI 0.159.3 answers the first message
  of a TUI with a second turn, on a thread of its own, that makes the task title; its
  input is Codex's instruction followed by the user's whole message, marker included, and
  its `notify` can arrive first (issue #61, 2026-10-02: `{"title":"READY"}` recorded as
  the result of two short initial requests out of six; a long prompt is cut in the title
  input and loses the marker). Any turn that Codex starts itself can quote the prompt in
  the same way. Do not weaken the header requirement to a substring match.
- With `/ide` on, the TUI puts its IDE context in front of the user's message: a first
  line `# Context from my IDE setup:`, the context, and a line `## My request for Codex:`
  (codex-rs/tui/src/ide_context/prompt.rs, rust-v0.159.3; taken from the source, not
  observed live). That input is still Bridge's turn. Recognise this wrapper exactly and
  no other; a new wrapper needs its own evidence from Codex's source or from a recorded
  payload.
- The header is framing, not provenance: a turn that copies the framed prompt verbatim,
  or an answer that ends with the marker, would still be accepted. Replace the
  correlation when Codex reports the thread of the TUI before the first turn, or gives a
  turn an identity that the launcher can choose.
- When a correlation question comes up, record the raw `notify` payloads with a temporary
  build and read them; do not reason from the result alone. The two payloads of #61
  looked alike in everything but the thread, the input and the answer.

## Current Windows Surface Boundary

- A managed console on native Windows is a tab of Windows Terminal, and a console window
  of its own only when no tab can be had: `wt.exe` is not on an absolute `PATH` entry, the
  desktop has no shell window, a path is one that Windows Terminal would alter, or no tab
  host offers a root in time. The request for the tab is withdrawn before the console
  window is created, and a surface is bound only once, so a session never runs in two
  surfaces; an empty tab can exist beside the console window until its host notices.
- `wt.exe` creates the tab's process itself. The tab therefore runs `native-console-host`,
  which creates the same PowerShell root, suspended, in the tab's console, offers its
  attested identity through the session directory, and starts it only after the launcher
  has bound the surface. Keep the root, its command line and its environment identical to
  the console-window path, so that no control path depends on the host. Replace the host
  when Windows Terminal can adopt a process that its caller created.
- The tab host creates the root inside a job that ends it with the host, and releases
  the job only after it has started the root, so a host that dies before the root runs
  leaves no process. Whether the root is started is decided once, by whoever creates
  `console-host-decision` first: the host to start it, or the launcher to give up an
  offer it had accepted. Keep that decision atomic. A launcher that decides first ends
  the suspended root. A launcher that finds the host's decision ends nothing: the
  surface counts as started and stays bound, the launch goes on to wait for the
  provider, and a launch that fails there is fenced as on every platform. Do not end a
  console during startup with the close helper: it ends the processes it saw, and a root
  that has just started is about to start more. Never end a started root alone: a later
  close finds the console by its root. A root that was never started has reached no
  console, so a close cannot attach to it; the close ends it alone only when the
  session's own records show that the wrapper never ran (no owner record, no spawn
  attempt in the launch receipt).
- By default the tab goes to the window named `agent-bridge`, never to a window the user
  works in. Windows Terminal selects a new tab, cannot create one unselected, and offers
  no way to select the earlier tab again, so a tab in the user's window takes the keyboard
  inside that window for good (measured 2026-10-01, Windows Terminal 1.24). Only the
  user's own `settings windows-tab-window current` sends the tab to the most recently
  used window (`-w 0`). Do not change the default, and use the dedicated window when the
  settings record cannot be read.
- A new surface gives the keyboard back. Windows Terminal honours a show-without-activating
  request only for a window that `wt.exe` creates while another application is in front;
  it brings an existing window to the front whenever a command line is dispatched to it,
  and creates a console handed to it as the default terminal in front when one of its own
  windows already is. Give the foreground back only from the window that hosts the
  surface, and never guess that window: a process inside the console reads it from the
  console's own window (the tab host before it starts the root, the `window` control
  helper for a console window, whose console window belongs to the terminal's console
  host and not to the root). A terminal window that merely appeared during the launch may
  be one the user opened. A window the user goes to during the launch is remembered and
  gets the keyboard back from then on. The surface's own window keeps the keyboard when
  it comes to the front again later than a window does on its own (200 ms after the
  keyboard was given back; 54 ms was measured): the user selected it. Earlier than that
  the launch cannot tell the user from the window and gives the keyboard back, because
  leaving it in a session the user did not choose is the worse mistake. The foreground
  is watched by a thread of its own, so that this time is measured while the launch
  waits on files and processes. Never join that thread without a bound: giving the
  foreground back goes through the input queue of the window that holds it, and a window
  that has stopped answering can hold the call for good. The tab host discards the
  console input typed before it starts the root: an Enter would answer the first dialog
  the provider shows. A console window has no host, so a key typed while its window is
  in front reaches the console.
- Windows Terminal keeps a tab whose own process ended with a failure code. The tab host
  leaves the tab's console once the root has attached to it, and confirms the start only
  then, so that closing a session, which ends its console processes with a failure code,
  still closes the tab. A close that finds the host still in the console ends it last
  and without a failure code; it recognises the host by the pid and identity that the
  host records about itself in the session directory (`console-host-process.json`). The
  host offers no root before it has written that record, and a close that finds the
  record but cannot read it ends nothing. A host that is still there a second after the
  root has ended is stalled; the close ends it, without a failure code, so that no tab
  outlives its session.
- A close ends the console processes it found when it attached. A process that the
  session starts while it is being closed can survive the close; this is unchanged and
  holds for both surfaces.
- Windows refuses to replace a file while any other handle to it is open, and a launcher
  polls the records that a wrapper replaces. Replace such a record only through
  `persist_record`, which repeats the rename for a bounded time.
- The console launch passes its command to PowerShell as one double-quoted `-Command`
  argument. The bootstrap must not contain a double quote: 0.0.8 put one there, and every
  launch on native Windows was refused before a console existed. The Windows test that
  runs the bootstrap through the launch's own command line guards this.
- A full-width character fills two console cells and is returned once, so a screen row
  can hold fewer characters than cells. Do not treat a short row as a failed read.

## Trust Store Boundary

- A provider trust store, and a Bridge consent record, is evidence only when nobody but
  the user can change it. On Unix that is the owner and the mode. On Windows it is the
  owner and the access list, both read from the handle that the content is read from: the
  owner must be the token's user or the token's default owner (the Administrators group
  for an elevated process, which is what a GitHub Windows runner is), and no access rule
  may let another account write, append, or change the access list or the owner. The
  system, the Administrators group and `OWNER RIGHTS` may. A missing access list and a
  rule that is neither a plain allow nor a plain deny are refused.
- The check describes the store at the time of the read. It cannot tell who wrote the
  content earlier, and it does not exclude a writer that has the store open. Do not open
  a provider store without write sharing to change that: Bridge polls Agy's store every
  100 ms while Agy saves an approved decision, and a provider's own write would fail.
- The gate that withholds an initial paste on native Windows asks another question than
  workspace consent: not whether the user approved this exact workspace, but whether the
  provider takes input now. For Codex an exact project entry is not the only way past
  the trust dialog. Codex CLI 0.159.3 takes the trust of a workspace without an exact
  entry from its repository root, found by reading `.git` itself and checking a linked
  worktree's registration; it shows no dialog then and saves no exact entry, and "Trust
  and continue" in a subdirectory saves the repository root (measured 2026-10-02;
  `--yolo` does not suppress the dialog). A wait for the exact entry never ended there
  (issue #60). The Codex adapter therefore also opens the gate on positive evidence from
  the screen: the empty composer row, which Codex draws only when no onboarding screen
  and no view of the bottom pane is active. Do not infer the absence of the dialog from
  Codex's configuration, from Git, or from a screen that merely lacks the dialog's
  words: an empty screen is not evidence, and a wrong inference pastes an Enter onto
  "Trust and continue". Consent that is shared with another provider stays exact.
- A wait for workspace trust ends when its session has ended, and looks at the session
  before it looks at the evidence: the launcher of a closed session must not wait until
  its deadline, and an ended session is never pasted into. This holds for the Codex wait
  and for the Agy wait.
- Create a store fixture in a test the way a private record is created
  (`write_private`, `write_json_atomic`). A plain write inherits what the temporary
  directory allows, and on a PC with the Codex sandbox that directory lets another
  account modify its files (observed 2026-10-02: `CodexSandboxUsers`), so the fixture is
  refused there and accepted on CI.

## Observation and Self-Test Boundary

- A query observes and nothing else: `inspect`, `inspect --timeline`, `result`, `search`
  and the read-only form of the sessions query never repair, recover, resend, close or
  write. A new query reuses the readers and the publication rules of the existing ones,
  so that the same request has the same state everywhere.
- The timeline shows what the records hold. It reads every record once for one answer,
  strictly as UTF-8 and within a bound, reports a record it could not read as such, and
  fails when a record changed while it read. What it derives is marked: the request
  summaries are what `result` reports at the time of the query and carry `derived_from`.
  Do not add an entry for something no record holds.
- `created_unix_ms` of a receipt and of an event is optional only when a record is read,
  so that a damaged record can be reported with a reason. Every writer writes it, and no
  path may publish an event without it: the completion-journal validation rejects one.
- The elapsed time is what Bridge observed between two records. Do not present it as
  model time or billing time, and do not collect provider usage from a signal that the
  provider does not document as usage.
- The self-test runs in the ordinary state root by default and in a private directory
  only with `--isolated` (owner's decision, 2026-10-02): its purpose is to check the
  installed setup as it is. It closes only what it created, bounds every command it
  runs, never resends after an uncertain delivery, and never deletes a record. It
  follows `ask`'s workspace-consent rules (owner's decision, 2026-10-06): verified
  consent for the exact workspace is reused through the provider's own approval, as
  in `ask`, and without it the self-test approves no workspace trust itself. An
  explicit `--yolo` only forwards the provider's own bypass option. `--isolated`
  separates Bridge's records, not a provider's trust store. Nothing else may call it.

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
  tag-push checkout rewrites the local tag ref to the commit, so only the remote still
  holds the tag object. Do not replace the API check with a second fetch: the checkout
  keeps no credentials, and the check must not depend on the repository's visibility.
  Only validation resolves the tag name; later jobs check out the validated commit SHA,
  and publication re-checks that the remote tag is still the validated tag object,
  because a tag can be moved while the run is in progress.
- Release scripts receive workflow expressions through `env`, never inline, so the policy
  tests can execute the exact scripts against fixtures.
