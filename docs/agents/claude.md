# Current Claude Boundary

- TAB reads Claude's workspace entry first and checks its Git scope only for an accepted entry.
  A prospective Claude grant from another provider and a Claude dialog key also require exact scope.
  A `.git` file or directory in the launch folder makes that folder its own top level for this
  check, without running Git or interpreting `core.worktree`.
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
- `--dangerously-skip-permissions` does not decide whether Claude shows its workspace trust
  dialog: on macOS 2.1.319 started without it and 2.1.321 shows it (2026-11-10), and native
  Windows shows it since 2.1.302. Do not skip the guarded answer from the bypass flag or a
  version. A macOS launch under bypass with verified consent answers a dialog that is on the
  screen and stops waiting when the completion module reports `session.start`, because a
  session that started without the dialog never writes its trust store; a launch without
  native completion has no such report and leaves the dialog alone, as does a surface that
  takes no guarded key (its screen read is refused). An unanswered wait under bypass that
  reaches the deadline is not a trust failure: it ends as not sent with the readiness
  timeout, which names the dialog as a possibility only from Claude's own store. After an answer
  only the store confirms trust. Replace
  the start signal with an official workspace-approval capability or launch option. The macOS
  owner check is in `docs/verification/2026-11-10-macos-claude-trust-dialog-under-bypass.md`;
  native Windows keeps its store-only wait and was not run.
- Claude Code 2.1.322 adds a paragraph to the workspace trust dialog when the folder's
  `.claude/settings.local.json` pre-approves tool permissions ("This folder pre-approves N
  tool permissions ... These will apply without asking"). "Yes" there also accepts those
  pre-approvals, so the guarded answer takes this variant only for a session launched under
  bypass, where Claude asks for no tool permission anyway (owner's decision, 2026-11-10),
  and only in the observed form: the count 2, the plural "tool permissions", that file
  name, and exactly two permissions (`Tool` or `Tool(rule)`, no whitespace) joined by
  " and " before the closing sentence. Without bypass the variant stays unanswered. Do not
  widen the matcher to an unobserved wording, a longer list whose separators were never
  seen, or another added paragraph: such a screen stays unanswered and the launch fails at
  its deadline with the readiness diagnostic. One permission, three or more, a permission
  wrapped across rows, another settings file and native Windows were not observed.
- Claude Code v2.1.224+ provides official cross-session messaging on macOS and Linux,
  including WSL 2. Native Windows support is official from v2.1.234 and uses a
  per-session named pipe. Prefer the official path only when every runtime availability
  gate passes.
- Use the official `ListAgents` and `SendMessage` path on every supported TAB
  platform. Do not restore the removed native-Windows `--print --resume` supervisor or
  silently fall back to terminal input when official messaging is unavailable.
- New macOS, Linux, and native Windows launches, plus Windows reopens, select `native_v1` completion
  before spawn from the recorded stable Claude version, at least 2.1.302, and load TAB's session-local
  completion module through Claude's official `--plugin-dir` flag.
  Actual `session.start` readiness is required before input; a version or plugin
  flag is no proof that the module ran. Missing readiness refuses delivery without a
  Stop fallback or a provider trust/policy override. Initial and follow-up prompts use
  `ListAgents` and `SendMessage`; argv carries no initial prompt and new settings add no
  classic TAB Stop hooks. The version floor establishes eligibility, not runtime readiness.
- Native completion observes only main-loop `turn.start` and `turn.complete`. Bind the
  whole expected message inside one anchored peer envelope to the live claim under the
  lifecycle lock, permitting only Claude's reversible insertion of one backslash after an
  ASCII `<` that the expected body does not already follow with a backslash, then keep that
  binding immutable. Claude 2.1.302 quotes protected opening and closing tags before turn.start;
  reserve that expansion at admission without
  reproducing its protected-tag grammar. Compare start/completion conversation
  and turn identities with each other, not the startup conversation, which `/clear` may
  change. The official turn id stays adapter-private; core `turn_id` must remain the
  TAB correlation id because delivery waits for it. Replace envelope matching when
  official SendMessage returns an addressed target-turn identity.
- The frame is the caller's prompt, one line `End your final response with this exact line:`
  and the marker (#396). Claude's own peer envelope names the sending session and the request
  receipt keeps the Request source, so the frame carries no delegation header, Source line or
  protocol label. The pending-record check accepts that frame and the start an earlier binary
  wrote (`[Agent Bridge native delegation]`, a `Source:` line, the marker at the end), which a
  request in flight across an upgrade still has. An earlier binary's check requires that start,
  so a launch recorded without `compact_prompt` keeps it ahead of the frame on every request:
  its completion hook can still be that binary. Completion depends on the exact marker suffix,
  not on the wording. The [Linux Herdr check](../verification/2026-11-18-linux-herdr.md) covers
  new sessions and a follow-up to a v0.4.45 hook.
- Classic Stop and StopFailure never settle a native-mode claim: any other Stop handler,
  including the user's own, can block that stop and continue the turn. A nonempty normal
  bound answer without the exact suffix marker is a retained candidate, never a Result.
  A marked bound answer settles normally. A different main-loop turn may settle only with
  a valid candidate of the exact current claim, pending request and immutable binding, in
  the same conversation, and a fully validated nonempty normal answer with that suffix.
  Record both official identities privately before publication; keep the core TAB id
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
  index; do not infer a turn from `tool.call` alone, which lacks that identity (deflection
  below ties a call to a step only through its streamed tool chunk's id). Pass streamed
  chunks and the response unchanged. Capture the conversation before awaiting the step,
  then require the exact original immutable binding and current Claim under the lifecycle
  lock. A continuation not yet attributed at completion supplies no question authority.
  One Claude-owned 64 KiB record preserves a prefix and monotone incompleteness; a duplicate
  step/tool position is idempotent and conflicting content marks it incomplete. A repeated
  model step, a non-tool-use response carrying a question, or a lower hook's rewrite also
  marks incompleteness. Hooks above TAB can change the output after observation. Historical
  reads use the selected Receipt under the query's lock. Emitted means neither shown,
  pending, executed nor answered, and no query changes the Claim or permits a pending tell.
  Only question text and option labels are projected; no answer or permission is supplied.
  The question helper gets two 5-second attempts; a failure logs unavailable observation
  and passes the stream's original response through. This adds no dialog-latency guarantee.
  Replace this observation path when Claude offers a verified addressed question lifecycle.
- Question deflection is a fallback in the same question plugin (#340): no person is at a
  managed session's dialog, and the owner ruled out a hook that waits for the caller's
  answer. The state root's `claude-question-deflection` setting (default on) is read once at
  launch; the adapter writes the module's switch and `claude-question-mode.json`, and every
  later read uses that record. With it on, a `tool.call` hook refuses a main-loop
  `AskUserQuestion` call raised by the engine with one fixed reason: end the turn with the
  question, end it with the marker line the request ends with, expect the answer as a
  follow-up `tell`. The reason names no frame label, so it fits every TAB frame.
  `tool.call` carries no turn identity and `turn.step` tool uses carry no call id, so a call
  is refused only when its `tool_use_id` matches a tool chunk this module passed in the
  observed main-loop step; the chunk's position among the step's tool chunks is its tool-use
  position. Claude 2.1.318 can start the tool while the step still streams, so the refusal is
  attributed after that step's engine/core return: `question.deflected` names the refused
  questions and marks the retained item at that position only when the return and that item
  both hold the same question text and option labels (a repeated step can keep an earlier
  question there), and otherwise marks the record incomplete; it never creates an item, and setting the
  flag is the one change a saved item may receive. Recording follows the emission rules
  (bound turn of the current Claim under the lifecycle lock, never a Claim or outcome change).
  Subagent calls, a plugin's `$.ui.ask`, unobserved calls, a module that is not loaded, and a
  hook that throws or overruns its budget all leave the dialog: no `.catch` refuses in their
  place. A failed record still refuses and leaves no flag, as does a refusal outside the bound
  turn: an absent flag is not evidence that no call was refused. A step whose stream breaks
  after a refusal marks the record incomplete. Claude fires settings `PreToolUse` hooks beneath
  plugin `tool.call` hooks, so a user's own `PreToolUse` hook for `AskUserQuestion` does not
  run for a refused call; only the setting restores it. A second dispatch of a refused call
  gets the same refusal, recorded once. The flag proves a refused call, not that the Result is
  a question or what the child wrote after it; `wait --questions` can return the emission
  before its flag. Replace the stream correlation when `tool.call` carries its turn and step
  identity or step tool uses carry their call id, and the fallback when Claude offers a
  verified addressed question lifecycle.
  The macOS owner check with Claude Code 2.1.318 (one question, the follow-up answer, and the
  same request with deflection off) is in
  `docs/verification/2026-11-05-macos-340-claude-question-deflection.md`; native Windows, a
  user's own `PreToolUse` hook, and subagent or plugin questions were not run.
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
  the Claude adapter). TAB is normally invoked from inside a Claude Code session,
  and a `claude` that inherits `CLAUDE_CODE_CHILD_SESSION` treats itself as a nested child:
  it never registers its cross-session inbox, so `ListAgents` cannot find it and delivery
  fails (issue #42, observed 2026-09-24 with Claude Code 2.1.281). The removal list is
  adapter-owned launch configuration; the shared module only applies it. Provider
  commands (managed launches, version and doctor probes, messengers and the Codex queue)
  keep their own adapter's removal list. An adapter may select another adapter's owned
  list by reference, never by copy: Claude, Agy and Pi select the Codex caller markers
  (see Current Codex Boundary). Codex, Agy and Pi also select the Claude-owned caller
  identity subset for probes and launches, and Codex uses it for its queue (#335).
  That subset includes session ids, the child-session marker, pid, inbox credentials,
  attended-session state and the companion ids; it excludes `AI_AGENT`, `CLAUDE_EFFORT`,
  `CLAUDE_CODE_ENTRYPOINT` and `CLAUDE_CODE_EXECPATH`. These execution hints stay inherited
  in directly spawned non-Claude provider commands. Claude's full removal list and
  own-origin diagnostic remain unchanged. Non-provider commands on macOS and native Windows,
  including terminal automation that can start an application and helpers, drop the union
  of the adapters' caller-session markers and TAB's state directory, session directory,
  session id and executable variables. A child that needs one receives it explicitly. On
  Windows this includes `wt.exe`, its tab host, both PowerShell roots and console control
  helpers. The console control helper explicitly receives the selected state root;
  the bootstrap sets it for the wrapper, which supplies the new session directory, id and
  executable to the provider and its hooks. An already contaminated terminal application
  must be restarted; child cleanup does not change its existing environment.

