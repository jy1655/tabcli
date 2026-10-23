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
  `src/native/provider/<provider>/` (or a single `<provider>.rs` file).
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

- Native Windows workspace approval is a bounded best-effort console fallback, derived
  from Claude Code 2.1.302's 150 ms input guard. The exact full dialog must be observed
  unchanged for one second before one combined Down/Enter response (or Enter when Yes
  is already selected), under an idle renderer and no concurrent input. Equal pixels do
  not identify a React generation, and the exact screen check is not atomic with Claude
  consuming input. A delayed remount can ignore input or reset selection to No before
  Enter and end Claude. Neither timing, a sent batch nor queue drain confirms trust:
  only Claude's private trust store does. Never retry uncertain input or answer the
  dangerous-mode warning. Replace this fallback when an official addressed workspace
  approval capability provides acceptance; the pinned source is not a runtime version gate.
- Claude Code v2.1.224+ provides official cross-session messaging on macOS and Linux,
  including WSL 2. Native Windows support is official from v2.1.234 and uses a
  per-session named pipe. Prefer the official path only when every runtime availability
  gate passes.
- Use the official `ListAgents` and `SendMessage` path on every supported Agent Bridge
  platform. Do not restore the removed native-Windows `--print --resume` supervisor or
  silently fall back to terminal input when official messaging is unavailable.
- New macOS and native Windows launches, plus Windows reopens, select `native_v1` completion
  before spawn from the recorded stable Claude version, at least 2.1.302, and load Bridge's session-local
  completion module through Claude's official `--plugin-dir` flag.
  Actual `session.start` readiness is required before input; a version or plugin
  flag is no proof that the module ran. Missing readiness refuses delivery without a
  Stop fallback or a provider trust/policy override. Initial and follow-up prompts use
  `ListAgents` and `SendMessage`; argv carries no initial prompt and new settings add no
  classic Bridge Stop hooks. The version floor establishes eligibility, not runtime readiness.
- Native completion observes only main-loop `turn.start` and `turn.complete`. Bind the
  whole expected message inside one anchored peer envelope to the live claim under the
  lifecycle lock, permitting only Claude's reversible insertion of one backslash after an
  ASCII `<` that the expected body does not already follow with a backslash, then keep that
  binding immutable. Claude 2.1.302 quotes protected opening and closing tags before turn.start;
  reserve that expansion at admission without
  reproducing its protected-tag grammar. Compare start/completion conversation
  and turn identities with each other, not the startup conversation, which `/clear` may
  change. The official turn id stays adapter-private; core `turn_id` must remain the
  Bridge correlation id because delivery waits for it. Replace envelope matching when
  official SendMessage returns an addressed target-turn identity.
- Classic Stop and StopFailure never settle a native-mode claim: any other Stop handler,
  including the user's own, can block that stop and continue the turn. A nonempty normal
  bound answer without the exact suffix marker is a retained candidate, never a Result.
  A marked bound answer settles normally. A different main-loop turn may settle only with
  a valid candidate of the exact current claim, pending request and immutable binding, in
  the same conversation, and a fully validated nonempty normal answer with that suffix.
  Record both official identities privately before publication; keep the core Bridge id
  and use `claude_mod_marked_continuation` provenance. A selected continuation cannot be
  replaced by a different continuation after interrupted publication. Strip only the suffix.
  Bound interruption, API error, refusal and empty answer still fail; those outcomes on an
  unbound turn cannot identify this request and retain its claim.
  This marker fallback covers the observed unmarked background interim, not general request
  finality: it proves neither task-notification origin nor provider-owned parentage. An unrelated
  same-conversation turn repeating the exact marker can match, and a marked interim can still
  finish early. An unmarked genuine final can remain pending indefinitely; a caller timeout
  does not retire the claim. `inspect` observes the candidate under its reader lock, without
  retry, repair or publication. Inspect the provider surface before closing or diagnosing it.
  Replace this fallback when Claude exposes verified addressed request-final evidence or
  a provider-owned continuation relation; 2.1.308's installed turn types provide neither.
  Malformed or unmatched signals retain the claim and write the latest rejection reason to
  `claude-mod-diagnostic.json` in the session directory. Input injected into an unrelated
  running turn without its own exact start remains unbound. Combined queued input that is
  not the exact addressed frame also remains unbound. A 4 MiB UTF-8 JSON limit covers native
  wires; admission counts escaping and bounded peer-header overhead. An oversized bound final
  fails without truncation; an unbound oversized signal has no marker and cannot settle the
  request. This route adds no cancellation API or usage mapping.
- An optional native question plugin observes structured `AskUserQuestion` emissions from
  a completed main-loop `turn.step`. Keep it separate from the completion plugin: Claude
  2.1.309 permits only one hooks module per plugin, and a policy refusal of question
  observation must not remove completion readiness. Evidence comes only from the final
  `next.trace` entry's engine/core return with `stopReason: tool_use`; an absent or short
  trace supplies no evidence. The event supplies the official turn id and step
  index; do not infer a turn from `tool.call`, which lacks that identity. Pass streamed
  chunks and the response unchanged. Capture the conversation before awaiting the step,
  then require the exact original immutable binding and current Claim under the lifecycle
  lock. A continuation not yet attributed at completion supplies no question authority.
  One Claude-owned 64 KiB record preserves a prefix and monotone incompleteness; a duplicate
  step/tool position is idempotent and conflicting content marks it incomplete. A repeated
  model step, a non-tool-use response carrying a question, or a lower hook's rewrite also
  marks incompleteness. Hooks above Bridge can change the output after observation. Historical
  reads use the selected Receipt under the query's lock. Emitted means neither shown,
  pending, executed nor answered, and no query changes the Claim or permits a pending tell.
  Only question text and option labels are projected; no answer or permission is supplied.
  The question helper gets two 5-second attempts; a failure logs unavailable observation
  and passes the stream's original response through. This adds no dialog-latency guarantee.
  Replace this observation path when Claude offers a verified addressed question lifecycle.
- An absent completion-mode record is a pre-upgrade legacy session, never upgraded from
  version inference. Its pending Stop completion remains readable, but new tells on both
  platforms are refused. Windows supports close/reopen recovery. On macOS, inspect the
  result and terminal, close when appropriate, then start a fresh `ask` with needed context;
  macOS reopen remains unsupported. Pending cleanup checks its request id under the
  lifecycle lock so an older sender cannot delete a successor's record. The module reports
  through existing native-hook stdin and core Report; at most one identical helper-wire
  repeat is allowed after the helper fails to start or exits nonzero, never another SendMessage.
- Do not downgrade the binary or `AGENT_BRIDGE_EXECUTABLE` while native-mode Claude sessions
  remain open; older binaries cannot interpret their completion protocol.
- Owner live checks on native Windows, 2026-10-19, Claude Code 2.1.302
  (`docs/verification/2026-10-19-windows-249-claude-native-completion.md`): ask and tell
  published the final answer after a diagnostic module blocked a draft Stop, including marker
  omission and 1,200 quoted delimiters; `disableAllHooks: true` withheld readiness and delivery.
  The configured user Stop command hook, steering, subagents, running-turn injection, combined
  queued input, Esc, API error, refusal, empty answer, `/clear`, worker respawn, close during a
  turn, non-yolo plugin trust and macOS runtime were not verified by these owner checks.
  Marker omission was supported by that earlier build; the background-continuation regression
  and revised marker requirement are recorded in
  `docs/verification/2026-10-22-windows-260-reverification.md`.
- macOS Claude 2.1.309 checks for configured Stop blocking, disabled readiness, ordinary
  permissions and background continuation are recorded with exact binary identities in
  `docs/verification/2026-10-23-macos-249-claude-completion.md`; they do not verify the 2.1.302 floor.
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
  keep their own adapter's removal list. Non-provider commands on macOS and native Windows,
  including terminal automation that can start an application and helpers, drop the union
  of the adapters' caller-session markers and Bridge's state directory, session directory,
  session id and executable variables. A child that needs one receives it explicitly. On
  Windows this includes `wt.exe`, its tab host, both PowerShell roots and console control
  helpers. The console control helper explicitly receives the selected state root;
  the bootstrap sets it for the wrapper, which supplies the new session directory, id and
  executable to the provider and its hooks. An already contaminated terminal application
  must be restarted; child cleanup does not change its existing environment.

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

- The Pi-installed extension reads core `turn.claim` and `cancel.json` read-only as a
  declared exception to the adapter-private record boundary. The whole UTF-8 claim,
  trimmed, is the current Claim token and currency guard; a changed or absent token
  ends correlation and cancel authority.
  Cancel is a JSON object with schema 1 and a string `claim_token`; other fields are
  ignored and parse failure supplies no intent. Claims can end through paths the adapter
  does not observe while Pi may still run (2026-10-15 critique). A Pi-owned copy would
  require retirement notification on every release path and a two-record publication
  protocol to avoid retaining abort authority. Tests execute the extension with real core
  writer output and round-trip its output through Rust readers. Replace these core reads
  when Pi provides an addressed interruption endpoint and Bridge has an adapter claim
  retirement boundary.

- Native Windows Pi input has no verified addressed first-party endpoint with per-turn
  acceptance. Bridge requires the installed extension's actual schema 2 startup record
  with `input_ack_schema: 1` before one console attempt, never a version inference.
  The extension matches the complete framed prompt's SHA-256 and UTF-8 byte length to
  Pi pending state at `before_agent_start`. Reading the whole trimmed core `turn.claim`
  at the correlated `agent_start` is also a declared read-only exception: it witnesses
  historical submission, without acquiring result or cancel authority. Latch one receipt
  publication attempt per claim before writing `pi-input-accepted-<claim>.json` through
  exclusive temporary creation and rename. Retirement after the witness read or a Pi
  conversation change does not invalidate that history. Retain receipts through later
  installs, rollback and close; they grant no input, result or abort authority. Sent
  requires a matching receipt within the original deadline. Read it before interpreting
  retirement or close, then re-read once; later publication can still leave Uncertain.
  Never resend text or Return. This fallback assumes no concurrent input and cannot
  distinguish a manual exact copy of the framed prompt. Keep open sessions on the same
  or a newer Bridge binary, including `AGENT_BRIDGE_EXECUTABLE`: older follow-ups can
  strip the startup input capability. Replace this fallback when Pi exposes addressed
  interactive input with per-turn acceptance identity.

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
- Usage comes only from the signal Pi documents as usage: the `usage` of every assistant
  message, which the extension sums over every `agent_end` of the correlated claim until
  settlement (`source: pi_extension_agent_end`, `scope: request`). Tool-result usage,
  compaction summaries and Pi's computed `cost` are excluded; `reasoning_tokens` only when
  every message reports it; `coverage: complete` only when every run's final assistant
  message stopped ordinarily. A malformed operand or sum becomes a bounded `usage_error`
  and no partial sum. Usage never changes the outcome, and the size policy drops usage
  (`usage_error: "omitted: event size"`), then the usage error itself, before it fails a
  message that fits on its own.
  The event fields are optional and omitted when absent, so an older binary reads a new
  event but drops the fields and sees a journaled completion as `Mismatched`: do not
  downgrade the installed binary or `AGENT_BRIDGE_EXECUTABLE` while a session whose turn
  this binary journaled is open; upgrading is supported. Replace the accumulation when Pi
  reports a turn's usage through an addressed first-party signal.

## Current Codex Boundary

- The injected Windows initial paste is a bounded best-effort console fallback (#124),
  pinned to Codex 0.163.0. It assumes no concurrent input; screen snapshots are not atomic
  with TUI transitions and placeholder counts are not content receipts. Non-injected or
  missing launch records retain the legacy console route, including its consent-only limit.
- Codex groups plain input at 8 ms intervals, flushes on Windows after 60 ms, and collapses
  individual pastes over 1000 Unicode scalars. Active-burst Enter and Shift+Enter insert LF.
  Applying a complete paste clears the 120 ms suppression window: no 150 ms proof exists.
  A visible prefix with input still pending can absorb Return into a later burst.
- Before writing, require the positive empty composer and a 20 s reserve. Write/drain body
  text only and verify its scalar count, then wait 150 ms before writing/draining LF plus the claim
  marker and verify that count, with no screen read between writes. The final exact inline
  marker is completion evidence under ordered console/reader/composer processing; its
  applied paste clears burst suppression. Wrapped/scrolled body rows need no positional
  matching. If the last segment is a placeholder, require the Codex-normalized framed scalar total
  from placeholders plus inline text (physical rows joined with LF): cardinality only,
  not a content receipt. Repeated labels use ` #2` outside the bracket. A dialog/picker
  veto precedes success; an unobserved/clipped marker or ambiguous fallback never submits.
  The request has one absolute deadline, and Sent still requires the claim binding.
- The 150 ms split gap only encourages the 60 ms idle flush; it proves nothing.
  The exact inset-stripped marker row ends composer content; subsequent rows still undergo
  the UI veto. Numbered pinned menu labels and confirmation hints anywhere, and any
  column-zero `› ` row outside the span, remain unresolved UI; the UI recognizer is pinned
  to Codex 0.163.0 forms at this PC's 120-column managed Windows Terminal tab and console
  window widths, the upstream 40-column snapshot and the 43–47-column sandbox wrap.
  Otherwise use the first recognized footer (workspace-qualified model line, warning,
  shortcut/queue/interrupt/edit/quit hint or context/token count), trimming separator blanks
  but preserving internal blanks. No marker and no recognized footer remains ambiguous.
  Count normalization is CRLF/CR to LF and control/CSI sanitization, not blank-line collapse
  (Codex rust-v0.163.0 paste_input.rs:145-176; history_cell/messages.rs:25-85).
  Bridge trims source prompts before framing: the live 25596/1094 estimates become
  25595/1093 there. Cursor-dependent blockquote fallback stays ambiguous; exact inline
  marker evidence remains usable. Unknown custom status lines require that marker.
- Recheck the current claim, session and deadline after blocking work and at input dispatch.
  The injected Windows helpers hold the lifecycle lock across the final claim/session/expiry
  check and each write; screen reads and drain waits remain outside that lock.
  Submit one guarded Return against the exact accepted full screen; a changed screen gets
  none. After body dispatch all unconfirmed failures are uncertain, except a helper's
  proven pre-write refusal. Never retry text or Return, including after late binding.
- Sent requires this claim's `UserPromptSubmit` binding, which proves inspection; acceptance
  and completion remain the transcript observer's job. Missing binding is uncertain with
  a final composer classification. Recovery is conditional: inspect result and surface;
  consider Enter once only with the complete draft, no dialog and no running turn, otherwise
  close or diagnose. Never claim unreadable content was observed.
- Replace this protocol when Codex exposes a verified Windows console paste event or
  newline-preserving input API. The legacy delayed-Return machinery remains until its
  supported non-injected configurations have a replacement.

- Codex 0.163.0 runs Claude-compatible lifecycle hooks (feature `hooks`, stable and on by
  default): a command hook receives one JSON object on stdin, runs in the turn's cwd with
  Codex's environment, and must never exit with code 2 (that blocks the turn). The legacy
  `notify` is `hooks/src/legacy_notify.rs`: the whole payload as one argv argument,
  spawned and not awaited, marked for removal upstream; on native Windows a payload past
  32,767 UTF-16 units never spawns (#127). Bridge therefore injects a `UserPromptSubmit`
  and a `Stop` command hook as one `-c hooks=` session-flag value and keeps `-c notify`
  as the fallback. The hook path is the official one; the notify path stays until every
  supported configuration has a verified replacement route (settings off, hook feature
  off, a launcher the shell cannot run, a transcript Bridge cannot observe, and Codex
  below 0.163.0 all still complete through notify), and removing notify needs that
  capability matrix, not an upstream deletion.
- Injection is decided at launch and recorded in `codex-launch-mode.json`: the recorded
  preflight version must be 0.163.0 or newer and not a prerelease, the state root's
  `codex-lifecycle-hooks` setting on, and the executable path one that cmd.exe and
  PowerShell run unquoted (the npm `codex.ps1` shim strips double quotes inside a `-c`
  argument, observed 2026-10-18, so the value uses TOML literal strings and no `"`).
  Every later decision reads that record, never current settings or a probe. The record
  says what was configured; only the session's own records say what ran.
- A non-managed hook runs only when `hooks.state.<key>.trusted_hash` equals the hash
  Codex computes from the normalized handler, and the state is read from the user and
  session-flag layers, so Bridge passes the two hashes it computes itself: SHA-256 over
  the compact, key-sorted JSON of the TOML identity `{event_name, hooks:[{type, command,
  timeout, async}]}` (`discovery.rs:777-793`, `fingerprint.rs:54-65`), pinned to the four
  values Codex wrote on Windows and macOS (R0, 2026-10-18; keys
  `C:\<session-flags>\config.toml:<label>:0:0` and `/<session-flags>/config.toml:...`).
  This replication is a fallback with a drift risk: a hash Codex no longer accepts makes
  the TUI open its hook-review picker before the composer on both platforms, and the
  picker holds the initial prompt (macOS awaits the review before `App::run`). Bridge
  never answers the picker. On Windows the Codex adapter requires the empty composer row
  before every initial paste of an injected launch, regardless of stored workspace trust
  or an applied trust override; a deadline names the picker as diagnosis. The doctor's
  `hooks/list` probe (`codex app-server`, JSON-RPC) reports the installed Codex's own
  trust status and hash without writing anything. `--dangerously-bypass-hook-trust` is
  never passed: it would run every user and project hook unreviewed.
- `UserPromptSubmit` gives the turn's identity before the answer exists: `session_id`,
  `turn_id` and `transcript_path`. It proves inspection, not acceptance; another input
  hook can still reject Bridge's input. Bridge binds the current claim to that identity
  (`codex-turn-<claim>.json`) only for a prompt that begins with the delegation header
  (directly or after the exact IDE wrapper) and ends with the pending marker, under the
  lifecycle lock against the live claim; an identical rebinding is idempotent, a
  different identity is a diagnostic. The marker alone never identifies a turn (#61), and
  a message typed into the TUI during the turn is steered into the same turn without the
  header (#65). The title turn is a temporary thread with hooks disabled, so it sends no
  hook payload, but its legacy notify can still arrive first; keep the #61 fixtures.
- `Stop` is not the end of the turn: every Stop handler runs while Codex decides whether
  to stop, and another handler's block continues the same turn (`session/turn.rs`).
  Bridge records the last Stop as a candidate (`codex-candidate-<claim>.json`) and never
  publishes from it. Legacy notify runs after that decision but before the turn function
  returns, so a post-turn compaction interrupted by Esc can follow a notify success with
  `turn_aborted`. For a bound claim notify is therefore evidence only
  (`codex-notify-<claim>.json`) and does not release the claim; for an unbound claim
  (no hook injected or no binding) notify settles exactly as before.
- A bound claim settles from Codex's own terminal record in the rollout transcript,
  observed by the adapter's session watcher (`LaunchPlan.watcher`): `task_complete` (or
  its `turn_complete` alias) and `turn_aborted` for the bound turn id, emitted after the
  hooks and notify of the turn (`tasks/mod.rs:829-866`) and persisted with a flush per
  append but no latency guarantee. The watcher reads the bound file only, from byte zero,
  newline-terminated records with strict UTF-8 and JSON, verifying the file's identity and
  its largest observed length around every read; replacement, shrink, a read failure, a
  malformed or oversized record or a malformed attribution field suspend that claim with a
  diagnostic (`codex-observer-<claim>.json`) and are never evidence about the prompt.
  A record past 8 MiB is reduced in place, not retained (#269: tool outputs carrying
  several base64 images, observed up to 50 MB): every string over 4 KiB becomes `""`
  after its bytes pass serde_json's `String` checks, the whole line passes strict UTF-8,
  and the reduced record must fit 8 MiB and parse as JSON. Only a non-message
  `response_item` after the verified `session_meta` is skipped, and only with a non-empty `payload.type` and no repeated
  top-level `type` or `payload` or `payload.type` (`Value` keeps the last duplicate). Any
  other record past 8 MiB, a user message with pasted images included, stays `oversized`:
  cutting it could hide the framed prompt. A fault in cut text suspends with its own reason
  as soon as it is read. Each running poll reads at most 256 KiB; the reduction at 8 MiB
  and the final parse are each up to 8 MiB of work within one poll. Acceptance is
  proved by one `user.text` message of the bound turn
  (`payload.internal_chat_message_metadata_passthrough.turn_id`) that carries the framed
  prompt; its absence is proved only by a complete, validated prefix, and then the request
  fails as not accepted. An abort fails with Codex's reason, a terminal error with its
  text, a null or blank answer as such; otherwise the answer is published after the
  marker is stripped, with `provenance` naming the signal. The transcript is a
  provider-owned record, not a documented contract: replace the watcher when Codex
  provides a terminal lifecycle event that carries the answer, delivers notify through
  stdin, or lets a TUI launched with overrides attach to the daemon (the embedded app
  server is in-process, so no external client can observe `turn/completed`).
- The same Cursor observes question emissions only from the bound turn's completed
  `AgentMessage` with `delivery: async` and validated structured questions, after accepted
  framed input. Only a request bound through injected Codex hooks can have this evidence;
  an unbound, notify-settled Claim cannot. Raw calls, tool output and prose supply no emission
  evidence. Optional turn attribution that is absent, malformed or different is ignored
  during byte-zero replay; an attributable item's malformed optional content marks incompleteness without
  suspending completion. One private `codex-questions-<claim>.json` holds a prefix that
  replay never rewrites. The 64 KiB limit covers the whole pretty-encoded record, Request
  identity included. Replay preserves prior items and monotone incompleteness. Publication
  checks the same locked Claim/status/pending predicate as binding plus exact Binding equality; failure
  never changes completion, and there is no backfill after Claim release, so a write that
  still fails at the terminal poll loses that evidence. The terminal flush reuses the
  successful terminal integrity checkpoint; nonterminal publication verifies separately.
  Queries use the selected historical Receipt under their existing lock. Ordinary single
  `result` shows content; status focuses its active Receipt or existing latest Result's Receipt;
  ordinary inspect shows per-request presence. Default wait, list, timeline, attachments and
  completion records retain their contracts. Opt-in `wait --questions` returns unseen retained
  emissions without settling the Request; its caller-held cursor is observation only.
  Emitted never means pending or answered, absence is
  unknown, and a later `tell` is another Request, not resolution. Tool availability depends
  on model/configuration and retained history. Replace this provider-owned transcript
  fallback when Codex provides a verified, addressed question event or lifecycle for the
  same injected TUI launch; never switch on a version check. Question component verification
  is recorded in `docs/verification/2026-10-19-codex-question-emissions.md`.
- Lifecycle completion live verification on native Windows, 2026-10-18, codex-cli 0.163.0
  (`docs/verification/2026-10-18-windows-127-codex-lifecycle-hooks.md`): bound claims settled
  from `task_complete` with and without a notify record (a 25,463-character prompt with a
  10,092-character answer left none), a steered TUI message joined the bound turn without
  rebinding, a user Stop hook's block continued the turn and only the final answer was
  published, a model error failed the request from `task_complete{error}` with no Stop and no
  notify at all, `settings_off` and `features.hooks=false` settled through notify, and an
  untrusted user hook held the picker, which the Windows gate named at the deadline. Not
  verified: Esc during post-turn compaction, a Bridge hash-drift build, and macOS. The
  native-queue `tell` passes the framed prompt as one `--message` argument, so on native
  Windows a follow-up above the command-line limit fails to start `codex queue` before anything
  is sent (os error 206 at 35,520 characters; #251): a send-side limit that the hook route does
  not change.
- When a correlation question comes up, record the raw payloads with a temporary build
  and read them: hook stdin, notify argv and the transcript lines. Do not reason from the
  result alone. The two payloads of #61 looked alike in everything but the thread, the
  input and the answer.
- Codex's background memory consolidation delegated through `tabcli ask` from its memory
  workspace and left the sessions open (issue #216: three on 2026-10-15 with tabcli 0.4.10 and
  codex-cli 0.162.1, three on 2026-10-08 with 0.2.6), and a `LOCAL.md` instruction did not
  stop it.
  Launch admission therefore refuses a workspace that is `memories` beside Codex's
  `config.toml` or lies inside it, for every target provider, unless the state root's
  `codex-memory-workspace` setting is `allow`. This is Bridge's admission policy for an
  observed path, not a fallback for a Codex transport; the Codex adapter only declares the
  location. Re-examine the default when Codex's behavior is verified to stop through an
  official control or an upstream fix; do not disable it from a version check alone.

## Current Windows Surface Boundary

- Codex owns injected paste evidence and guarded submission; see Current Codex Boundary (#124).

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
- Every session start assesses workspace consent for the new session from the current
  stores, `reopen` exactly as `ask`: the source session's assessment is diagnostic history
  of another session and is never reused as consent (#202). A start that skipped the
  assessment would leave `authorized` false for good, so the session could not take the
  guarded dialog answer that a fresh `ask` can. The assessment is part of the resolution,
  before anything is created, and create records it with the session before anything else
  binds to that session: a failed assessment consumes nothing, and a record that cannot be
  written fails the creation, which for a reopen releases the source reservation (found in
  review, 2026-10-17).

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
  provider does not document as usage. Usage a query shows is what the event records
  (`usage`, or `usage_reason` with the bounded `usage_error`); never estimate or total it.
- A delivery refusal (`Delivery::NotSent`) is settled by `settle_delivery_refusal_locked`
  as the receipt's failed event: Bridge-owned evidence of what was not sent, with
  `provenance: "delivery_not_sent"`, the refusal text as `error`, the session's provider
  from the manifest and no provider session or turn id. It is journaled through the same
  completion machinery under the one lifecycle lock, after a pending completion is
  recovered and only for the installed claim of a session that is not closed; a failed
  journal write leaves the claim installed. Every query reads it as `failed`; the timeline
  marks it `stage: delivery_refusal` and `delivery: refused_not_sent`; reopen accepts it.
  Historical receipts without an event stay `unresolved`: never infer a refusal from
  `status.error`, never repair such a receipt, and the reopened-conversation refusal in
  `reopen.rs` still leaves one on purpose.
- The self-test runs in the ordinary state root by default and in a private directory
  only with `--isolated` (owner's decision, 2026-10-02): its purpose is to check the
  installed setup as it is. It closes only what it created, bounds every command it
  runs, never resends after an uncertain delivery, and never deletes a record. It
  follows `ask`'s workspace-consent rules (owner's decision, 2026-10-06): verified
  consent for the exact workspace is reused through the provider's own approval, as
  in `ask`, and without it the self-test approves no workspace trust itself. An
  explicit `--yolo` only forwards the provider's own bypass option. `--isolated`
  separates Bridge's records, not a provider's trust store. Nothing else may call it.

## Current Attestation Boundary

- One rule decides whether a recorded incarnation is still the live process (`process_identity`):
  Live, Gone(Exited), Gone(Reused) or Unknown. Only the platform observer confirms absence: raw
  `ESRCH` on macOS, `ERROR_INVALID_PARAMETER` from `OpenProcess` or a signalled handle on Windows.
  An inspection failure of a process whose identity was recorded is Unknown. Never derive a death
  from the liveness bool when an identity was recorded: on Windows that bool turns every open
  failure but access denial into "dead". The bool answers only pid-only records.
- Unknown is not conservative by itself. Prune blocks, repair fails, the wait continues and the
  exact-intent close resume refuses. A dead owner must reach Gone through the observer, or every
  command on that session fails: "every unverified open failure is Unknown" would have made every
  dead Windows owner Unknown (plan defect found in review, 2026-10-15, #190).
- A reused pid is the verified death of the recorded incarnation and grants no authority over its
  surface. A changed control identity (controlling TTY, process groups) of a live incarnation is
  not a death. A second observation of the same pid is checked against the attested incarnation
  before its control fields are compared; a later snapshot with another birth is reuse (#190).
- Startup repair consumes the typed owner attestation too: only Gone fails an unconfirmed
  launch as an exited wrapper. Live with changed or unknown control is still live; process
  Unknown or an unreadable owner returns an error without retiring the claim. A missing owner
  can still be registering. The launch deadline independently fails startup, without calling
  the owner observer or implying process death.
- Attestation reads the owner value and OS evidence only and takes no session lock; launch repair
  and close call it under their own lock. Callers (prune, repair, wait, close resume, the
  terminal adapters, reopen, the Claude registry) map the four answers in their own vocabulary
  and keep their messages and authority.
- Windows: the attestation compares executable paths case-insensitively; record and surface
  handle equality stay exact. The observer opens with `PROCESS_QUERY_LIMITED_INFORMATION |
  SYNCHRONIZE`, so a process that grants only query access is Unknown, never absent. The
  image-path query of an exited process fails (os error 31, GitHub runner, 2026-10-15), so an
  exited process whose object is still retained is Gone through the signalled handle (#191).

## Change and Verification Rules

- Public documentation and default plugin metadata use English. Run `python3 scripts/check_language.py`;
  translations and Unicode fixtures have explicit exceptions described in `docs/testing.md#language-lint`.
- For provider behavior changes, add or update tests in that provider's adapter and prove
  that unrelated provider adapters do not need modification.
- For concurrency or lifecycle changes, reproduce the race or partial transition with a
  deterministic test before patching it.
- Run `cargo test --all-targets --all-features -- --test-threads=1`,
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check`, and
  `git diff --check` before claiming completion. Use serial test-harness execution for
  local, CI, and release checks; retain concurrency created inside individual tests.
- Finish every release with local build cleanup after publication verification, installation
  (if requested), and all checks. Follow `docs/releasing.md#local-build-cleanup`: preserve
  evidence outside `target/`, ensure no build or live session depends on its binaries, then
  run `cargo --config 'build.build-dir="./target"' clean --target-dir ./target` from the
  checkout root. Clean only this checkout's build output, never other worktrees, installed
  binaries, session records, a separately configured build directory or global Cargo caches.
  Report reclaimed space, or the dependency or verification gap that deferred cleanup.
- Tests requiring authenticated CLIs and visible terminal surfaces remain manual live
  tests; do not represent ignored live tests as runtime verification.
- A claim about Windows behavior is not verified until its `cfg(windows)` test has run on the
  Windows CI runner. Cross-target clippy proves compilation only, and a review's source-level
  mock of Win32 can contradict the system (2026-10-15, #191: the mock had the identity query of
  an exited process succeed; Windows refuses it).
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
