# Providers

Run an installed, signed-in `codex`, `claude`, `agy`, or `pi` through Bridge on macOS or native
Windows. Each session runs in a visible terminal surface; choose one from
[terminal support](terminals.md). Linux is unsupported.

“First-party CLI” means the provider's own command runs the conversation and tools under your
existing login. Bridge has no credential store, cannot attach an independently launched CLI,
and does not start, stop, or restart a provider's shared daemon. Provider transcripts and
permission settings remain separate from Bridge's records.

Bridge uses each provider's own launch, delivery, and completion mechanisms where available.
Where those are missing, the fallback stays specific to that provider and is meant to be removed
when the provider supplies the missing feature.

Bridge does not grant new workspace trust. With verified existing consent, it can answer Claude
or Agy startup trust dialogs in iTerm2, Terminal.app, and the native Windows console. In other
terminals, approve the workspace in the provider before launching through Bridge. Read
[workspace trust and consent](security-and-data.md#workspace-trust-and-consent) for the checks.
The [CLI reference](cli.md) lists the request and result commands.

## Versions and launch options

Install and initialize the CLI before launching it through Bridge. These are the minimum
versions Bridge accepts at launch; Codex follow-ups need a newer version, as explained below.
Omit `--model` and `--effort` to use the provider's defaults. Bridge forwards the listed flags for
`--yolo`; the provider's own configuration still determines its permission mode.

| CLI | Minimum launch version | `--model` | `--effort` | `--yolo` forwards |
| --- | --- | --- | --- | --- |
| `codex` | 0.147.0 | `--model` | `-c model_reasoning_effort="VALUE"` | See below |
| `claude` | 2.1.302 stable | `--model` | `--effort` | `--dangerously-skip-permissions` |
| `agy` | 1.1.12 | `--model` | `--effort` | `--dangerously-skip-permissions` |
| `pi` | 0.84.1 | `--model` | `--thinking` | `--approve` |

Codex's bypass flag is `--dangerously-bypass-approvals-and-sandbox`. Bridge adds no sandbox around
these flags. Pi also receives `--approve` when Bridge applies verified workspace consent; this
approves project resources and does not disable Pi's tool policy. Read
[security and data](security-and-data.md) before using `--yolo`.

For Codex models, Bridge removes a nonempty `openai-codex/` prefix. Claude maps `Fable5` to
`Fable`. Pi maps `Fable` to `anthropic/claude-fable-5`. Other identifiers pass through unchanged.
Use an identifier accepted by your installed provider; Bridge does not maintain a model catalog.

To ask about a project, run:

```sh
tabcli ask codex --workspace /path/to/project --prompt "Explain the project structure."
```

Replace `codex` with `claude`, `agy`, or `pi`. Install and sign in to that CLI first; the sections
below explain its workspace-trust requirements and delivery limits.

## Codex

Install and sign in to `codex`. You need 0.147.0 or newer to launch, and 0.149.0 or newer for
follow-ups. Open the project in Codex first and resolve its workspace-trust prompt.

On macOS, Bridge passes the initial prompt as a launch argument. On native Windows, it waits for
Codex's startup delay and either verified workspace trust or the empty composer before typing
when lifecycle hooks were not injected. For a session recorded as `injected`, Bridge always
requires the visible empty `› Ask Codex to do anything` composer with no workspace dialog or
hook-review picker. Stored exact trust and an applied trust override cannot skip that check.
A visible composer still passes with repository-root trust and no exact project entry. The gate
checks session termination before readiness and keeps the original request deadline.
If the legacy trust wait expires, the error says `workspace trust is not verified` and that no initial input
was sent. Check the managed surface and approve the workspace if you trust it. After a failed
initial request, close that session and run a new `ask`; `tell` requires a ready session.
An empty screen does not count as a ready composer.

Windows console initial paste encodes each LF as Shift+Return, keeping submission Returns
unmodified. This fallback assumes Codex's default keymap and an editable composer; custom
bindings, Vim normal mode, history search and disabled input are outside its supported scope.
Bridge does not interpret Codex keymaps. Tabs in this paste path are refused with
`replace tab characters with spaces`: no literal Tab encoding is proven to preserve text
independently of timing and UI state. Native queue follow-ups are unaffected. Bridge normalizes
CRLF to LF for `--prompt-file` before the input policy runs; inline `--prompt` is not normalized.
The input policy rejects any remaining CR.
Replace this fallback when Codex provides a newline-preserving input API or paste event on
Windows console. The [2026-10-12 Windows check][win48] measured newline preservation with
codex-cli 0.162.1: the recorded message retained 28 line feeds from a 23-line prompt, but lost
an emoji (#147): each UTF-16 surrogate unit was written as its own key-down and key-up pair, and
Codex's console reader (crossterm) combines consecutive surrogate events regardless of the key
state, so the release of the high surrogate broke the pair. A surrogate pair is now written as two
key-down events only; a BMP character keeps its key-down and key-up pair. Agy and Pi retain their
existing LF encoding and need separate provider evidence.

For Windows initial launches recorded as `injected`, #124 uses a bounded best-effort console
fallback pinned to Codex 0.163.0. After the positive composer gate, Bridge reserves at least
20 s of the original deadline at admission; this is not a guaranteed post-text budget.
It writes and drains the exact framed body as text only, verifies its scalar count, then waits 150 ms before writing and draining LF plus the claim
marker and verifies that count. There is no screen observation between these writes.
The final inline marker line is completion evidence under ordered console/reader/composer
processing: earlier input was processed before the marker, and applying its paste clears
burst suppression. Wrapped or scrolled inline body text does not need positional matching.
If the tail merged into a final placeholder, Bridge instead requires the sum of placeholder
counts and visible inline scalars (physical rows joined with LF) to equal the framed prompt's
Codex-normalized scalar count. This fallback is cardinality only, not a content receipt; wrapping, clipping,
scrolling and literal placeholder collisions can make it ambiguous or misleading.
Complete pinned dialog or hook-picker layouts veto success as Blocked. The addressed composer
starts at the first column-zero `› ` row unless that row is pinned UI evidence; its span ends
at the exact marker or, without one, the first recognized footer. Footer-shaped content before
the marker remains content. Any column-zero `› ` row outside that span is unresolved UI,
regardless of text or numbering. Anywhere on screen, numbered rows after `› ` or two spaces
whose labels begin with a pinned hook/trust option, and confirmation-hint fragments, are UI
evidence. An inset-stripped row is hint evidence if a ` · `-separated part (including a
terminal ` ·` after wrapping) ends in confirm/continue/skip/quit/back/cancel with any number
of preceding key tokens, the whole row ends in one of those actions after one or more key
tokens, the row is exactly one of those bare action words, or it contains `and create sandbox`.
Incomplete layouts with recognized UI evidence are Ambiguous. Ordinary numbered prose remains content, but a
numbered pinned menu label inside the draft (such as `2. Quit` or `1. Review hooks`) or a
hint-like draft line (such as `enter confirm · esc skip` quoted in a prompt) or a bare
action word on its own draft line (such as `quit`, `back` or `confirm`) is refused
as unresolved UI: rephrase the line or press Enter yourself after checking the surface.
Bridge then requests one guarded Return: the helper compares the entire accepted screen and
writes no Return if it changed. Injected text and Return helpers also check the addressed claim,
session status and original expiry under the lifecycle lock immediately at dispatch. This assumes no
concurrent human input; screen comparison is not atomic with a TUI transition. A clipped or
wrapped marker with no recognized footer remains unconfirmed.
The UI recognizer is pinned to Codex 0.163.0 forms at the 120-column console widths of both
the managed Windows Terminal tab and console window on this PC, plus the upstream 40-column
snapshot and the 43–47-column sandbox wrap; narrower or custom layouts and other torn-frame
shapes fall under this fallback's best-effort limits and are not guaranteed to be recognized.

The gap is a heuristic to let the 60 ms idle flush separate the body; it proves no
processing boundary and the merged-tail fallback remains. Parsing ends at the exact
inset-stripped marker row, excluding later rows from draft content while still checking them
for UI. Without that row, the first recognized
footer ends the content: model/status text ending in this session's workspace (case-insensitive
Windows path normalization), warnings, shortcut/queue/interrupt/edit/quit/plan-mode hints, context
percentage or token usage. Trailing separator blanks are dropped; internal blanks stay.
Unknown custom status lines, history search and overlays are not inferred as draft footers.

[Codex paste normalization](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/tui/src/bottom_pane/chat_composer/paste_input.rs#L145-L176)
maps CRLF/CR to LF and invokes
[control/CSI sanitization](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/tui/src/history_cell/messages.rs#L25-L85)
before counting Unicode scalars. It preserves triple LF. The real 25596→25595 and
1094→1093 differences come from Bridge's existing source-prompt trim before framing,
not Codex normalization. Blockquote expansion depends on insertion position and burst
boundaries, so a merged-tail fallback containing blockquote lines remains ambiguous;
an exact inline marker can still complete observation.

`Sent` means this claim's `UserPromptSubmit` binding was observed, not completion or final
acceptance: another input hook may still reject the turn, and the transcript observer decides
acceptance and result. A gate/reserve failure or proven pre-write refusal is `not_sent`.
After body input, count mismatch, ambiguous or unreadable evidence, claim/session changes,
tail failures, changed-screen refusal and missing binding are delivery-uncertain; Bridge keeps
the claim and never automatically repeats text or Return. Missing binding includes a final
composer classification: populated, positively empty, blocked, unreadable or ambiguous.
This also applies when a launch is recorded as injected but `features.hooks=false` prevents binding.
Check `tabcli result <session>` and the surface. Only if the entire draft remains in the composer
with no dialog or running turn, consider pressing Enter once. Otherwise close the session or
diagnose with `doctor`.

The **legacy console route for not-injected launches (#124)** retains the consent OR composer
gate, two delayed Returns and the empty-composer wait (up to 10 s within the request deadline).
Its consent-only admission does not prove that the surface is editable; it has none of the new
binding-based submission evidence. Missing launch records also use this explicit legacy route.

`tell` uses `codex queue --thread <UUID> --message <TEXT>` to address the same conversation.
Codex chooses a shared or embedded server; you do not need to start a shared daemon. Bridge waits
for Codex's exact queue acceptance, then for the turn's completion. Acceptance alone is not a
result.

A Bridge-launched Codex TUI runs on its embedded app server. Bridge passes its `notify` hook and
the verified workspace trust as process-local `-c` overrides, and Codex keeps a TUI with such
overrides off the shared background server (read from the 0.160.1 source; a warning was first
recorded with 0.159.3). When Codex's daemon auto-start is enabled, the TUI's startup history
says `Running without the shared background server: command-line configuration overrides (-c,
--enable, --disable, or --search) requires embedded mode.` That line is expected (#89): result
correlation and `tell` were verified in this mode. `/agents` in that TUI offers to start a
background server; Bridge never answers that dialog, and you do not need to either.

With Codex older than 0.149.0, `tell` fails before anything is typed and restores the session to
`ready`. Upgrade Codex and launch a new session. A follow-up also needs the thread UUID from the
first completed turn. If the queue is unavailable, run `tabcli doctor <session> --probe`
and inspect the reported prerequisite. Bridge never falls back to typing a follow-up: the
surface could be showing another thread or a picker.

With stable Codex 0.163.0 or newer, `settings codex-lifecycle-hooks on` (the default), and a
supported Bridge executable path, Bridge injects process-local `UserPromptSubmit` and `Stop`
command hooks. Both call this executable's `native-hook codex`, receive JSON on stdin, use a
30-second timeout, and run synchronously. Injection does not prove that hooks run: Codex's
feature flags, disabled handlers, trust and managed configuration still govern availability.
`codex-launch-mode.json` records the injection decision, reason and version at launch; later
settings changes do not change that session's readiness gate. `off` affects new launches only.
Older/unknown/prerelease Codex versions and unavailable launchers retain notify. On Windows,
the hook executable path must contain only ASCII letters, digits, `_`, `.`, `:`, `\` or `-`;
a path with spaces, Unicode or shell metacharacters makes hook injection unavailable.

The same `-c hooks=...` value supplies `trusted_hash` for only Bridge's two synthetic
session-flags keys (`user_prompt_submit:0:0` and `stop:0:0`). Hashes use Codex 0.163.0's
normalized event label and selected platform command, timeout 30 and async false; they depend
on this executable's path. Bridge writes no Codex config and sets no `enabled` flags. It leaves
unrelated trust state and user-disabled handlers to Codex's merge. Vector agreement proves
normalization, not execution; doctor warns when the recorded launch version differs.

Hash drift, or an unrelated untrusted hook, can open Codex's startup hook-review picker on both
platforms. Bridge never answers it. On Windows the positive gate withholds all initial input;
at the deadline it reports `not_sent`, naming the picker when the last readable screen contains
`Review hooks` or `Trust all and continue`, even if stale composer text is also visible.
On macOS the argument-delivered prompt can wait behind the picker: a result timeout and no
input-hook observation mean there is no observed initial-input acceptance, possibly because of
a startup review screen; they do not prove `not_sent`. Inspect the managed surface yourself.
`doctor SESSION --probe` can compare current hook trust/hashes; settings `off` is a fallback
for new sessions, not permission for Bridge to answer a picker or resend uncertain input.

An observed `UserPromptSubmit` binds the current claim to Codex's thread, turn and transcript.
The prompt must carry Bridge's exact delegation framing (or the recognized IDE wrapper) and
trailing marker. `Stop` records only the latest candidate, which can precede continuation or
compaction: it is neither a terminal signal nor a source of result text.
For bound claims, the transcript watcher is the terminal signal. It reads from byte zero for
each binding, verifies the session identity and accepted framed user input for that turn,
and settles only on a matching `task_complete`/`turn_complete` or `turn_aborted`. Null/empty
answers, provider errors, interruption and proved nonacceptance fail the request. It reads
strict newline-terminated UTF-8 JSON in bounded chunks, retaining partial tails; replacement,
truncation or malformed evidence suspends observation and leaves the claim pending.
Each running poll reads at most 256 KiB, and a record or partial tail is retained up to 8 MiB.
A longer record is reduced in place: strings over 4 KiB are validated and dropped, and the
reduced record must parse as JSON within 8 MiB. It is skipped only when it is a non-message
`response_item`, such as a tool output with images, after the verified session identity, with
intact and unrepeated discriminators. Any other record over 8 MiB, including a terminal record,
a user message with pasted images or the session identity, suspends observation as `oversized`.
The reduction and the final parse of such a record each take up to 8 MiB of work in one poll.
At 256 KiB per 200 ms poll, processing 50 MB has a polling budget of roughly 40 seconds,
before I/O and parsing overhead; this is an estimate, not measured latency. It never falls
back to notify for a bound claim. Replace this polling reader when Codex supplies an
addressed first-party terminal signal with equivalent identity and settlement semantics.

The same watcher observes Codex's emitted async questions from completed `AgentMessage`
items attributed to the bound turn, after it verifies accepted framed input. Only requests
bound through Bridge's injected Codex hooks can have this evidence; unbound, notify-settled
claims cannot. The optional `codex-questions-<claim>.json` retains an ordered prefix within
64 KiB of serialized JSON.
Duplicate ids keep their first content; conflicting or invalid items and the size limit mark
known incompleteness. Storage failures can lose optional evidence, including at completion.
They do not block the ordinary result. No record is backfilled after the claim ends.

Ordinary `result` exposes `question_emissions` during the turn and afterward. `status`
reports presence for its active or latest-result request, and ordinary `inspect` reports
per-request presence. Absence is unknown. Emission never means that a question is still
pending or has been answered. A Ready `tell` is a new request, without a resolution receipt.
Codex registers the tool only for a root session with its question configuration on and a
model whose catalog offers it. Codex keeps the item only under its paginated rollout history;
its legacy history policy can omit it. Bridge does not infer support from a version,
parse question-shaped prose, or accept a raw `function_call` as emission evidence.
Replace this transcript fallback when the same managed TUI exposes a verified official
addressed question event or lifecycle. `wait --questions` can return retained emissions to a
waiting caller before completion, with a caller-held cursor for resuming observation.
Answer handling and unsolicited notification of a caller remain deferred.
Component verification is recorded in
[the focused verification note](verification/2026-10-19-codex-question-emissions.md).

Unbound claims retain the `notify` fallback: Bridge accepts an assistant result ending with
the pending marker, or a completion whose string input begins with the delegation header and
ends with that marker. A steered TUI input may follow Bridge's prompt; the answer serves both.
These checks identify framing, not provenance: a copied framed prompt can still match. The
first committed result establishes the thread, which later results must name. Notify stays
until every supported configuration has a verified replacement or is explicitly unsupported,
including hooks off/disabled/untrusted, older versions, managed-only configurations and
unavailable launchers or transcripts. A bound transcript failure does not grant notify authority.
Do not use an older separate binary for commands or recovery on sessions journaled by this
binary, or downgrade the executable while they remain open; older readers lose provenance.

IDE-wrapper support comes from Codex's source and Bridge's tests. A live session with `/ide`
is not verified; see the [Windows verification record][win10].

On native Windows an unbound request that still depends on `notify` has a payload size limit.
Codex runs the notify command with the whole JSON payload as one argument, and that payload
carries the delivered prompt
(`input-messages`) and the whole answer (`last-assistant-message`). Windows refuses a process
command line of 32,767 UTF-16 units or more, so a long prompt with a long answer makes the spawn
of `tabcli.exe` fail with error 206 and the completion never reaches Bridge: the request stays
`pending` for good and the session stays `working` until the caller's timeout (observed
2026-10-11 with codex-cli 0.162.1: a 21,655-character prompt and a 12,586-character answer).
For that unbound route, keep the delivered prompt and the requested
answer short on native Windows: put long material in files in the workspace, ask for a short
written result, and prefer a file-based report. To recover, read the answer from Codex's rollout
record (`~/.codex/sessions/<date>/rollout-*.jsonl`), then `close-session` the session; do not
resend an uncertain turn. `tabcli doctor <session>` reports `codex_notify_payload_budget`, an
estimate of the pending turn's payload against the limit from the framed prompt's size, which the
pending-turn record keeps for the initial turn and for each follow-up as sized before delivery; the
real command line can be larger or smaller than the estimate, and text with many quotation marks is undercounted
because Codex's argument quoting escapes each of them. See [doctor](cli.md#doctor).

The native-queue follow-up has a send-side limit of its own on native Windows. `tell` starts
`codex queue` with the framed prompt as one `--message` argument. Before spawning the queue
client or writing its pending-turn record, Bridge measures the prepared command line, including
the launcher, arguments and Windows quoting, in UTF-16 units (excluding the terminating NUL).
At 32,767 units or more it refuses delivery with the measured size, limit and remedy; nothing
is sent, the request is recorded as `failed` with that refusal (`provenance: "delivery_not_sent"`)
and the session returns to `ready` (issue #251).
Shorten the follow-up and pass long material by path in the workspace. Codex's inner hop from
`codex.ps1` to `node` is unmeasured and can still fail below Bridge's limit. The lifecycle-hook
route does not change this send-side limit; the console-pasted initial prompt has no such limit
beyond the paste defects of #124.

`reopen` refuses closed Codex sessions. Bridge still needs checks for the thread's writer lock
and queued inputs before supporting resume; start a new session instead.

`cancel` is unsupported in this release's Codex integration. As of the 0.162.1 source
review, `turn/interrupt` needs a connection to the app server executing the turn; an
external connection to Bridge's embedded TUI server was not verified. The legacy `notify`
is not interruption acknowledgement, and follow-up queue progress after interruption is
unverified. Support needs an official connection to that running backend, correlated
interruption completion, and verified follow-up resumption.

## Claude Code

Install and sign in to stable `claude` 2.1.302 or newer. Resolve workspace trust in Claude before launch,
or use [existing workspace consent](security-and-data.md#workspace-trust-and-consent).
New macOS and native Windows launches, plus Windows reopens, require a recorded stable Claude
Code 2.1.302 or newer and load a session-local Bridge completion module through Claude's official
`--plugin-dir` flag. Bridge
waits for that module to report readiness before sending the initial prompt. Disabled hooks
(`disableAllHooks: true`, observed on Windows and macOS), unresolved plugin trust or provider policy can withhold
readiness; Bridge sends no prompt and never substitutes a Stop hook or terminal input.
The version floor establishes launch eligibility, not proof that the module ran.

On both platforms, Bridge delivers the initial prompt after launch through Claude's official
cross-session messaging. The prompt is absent from argv. Every follow-up uses that messaging path too:
a separate, nonpersistent Claude messenger model turn calls `ListAgents` to find your session
and `SendMessage` to deliver the prompt. Bridge removes inherited Claude session markers so a
launch from inside Claude does not create a nested session with an undiscoverable inbox; the list
includes the caller's session id and transcript path that the Claude Code Codex plugin sets
(`CODEX_COMPANION_SESSION_ID`, `CODEX_COMPANION_TRANSCRIPT_PATH`, #149).
On macOS, Bridge also keeps those markers and its own per-session variables out of every
other process it starts, including a terminal application it starts. An application already
started by an earlier version keeps them until it is restarted. The [2026-10-12 Windows check][win48]
verified scrubbing in the managed Codex process on both a Windows Terminal tab and a separate
console window. Only the Windows Terminal cold-start observation followed by a user-opened tab
remains open in [#111](https://github.com/jy1655/tabcli/issues/111).

Messaging must be available in the running CLI's backend, features, and settings, and discovery
must find the exact live local session. A recent version or saved inbound setting does not prove
those conditions. If delivery fails, run `tabcli doctor <session>` and read the messaging
checks. Bridge does not substitute terminal input.

A proven not-sent follow-up releases its claim and restores `ready`. A proven not-sent
initial delivery leaves the session `failed`; close it and start a new session. An uncertain
delivery keeps a still-pending request claimed. The initial-delivery error says
`could not be confirmed`; the follow-up error says `could not confirm delivery`. Do not repeat
that prompt. Use `tabcli result <session> --request <request-id>` to inspect the original
request, or explicitly close the session if you decide to abandon it.

The messenger model sees a per-request reference, not your follow-up text. A `PreToolUse` hook
supplies the exact addressed payload through `updatedInput`, and a `PostToolUse` report confirms
what ran. On new sessions, Bridge's completion module forwards Claude's official
`turn.start` and `turn.complete` events. Bridge binds the complete addressed prompt at
`turn.start` to the current claim. A normal answer completes the request only with the
exact delegation marker at its suffix.
Claude can quote protected tags in the peer body by inserting a backslash after `<`;
the comparison permits only that reversible insertion and admission reserves its wire space.
If a Stop hook blocks the stop and Claude continues the turn, Bridge publishes nothing until
the official `turn.complete`. Stored results retain Claude's conversation identity and Bridge's
request correlation id; the official turn id stays in the adapter's immutable binding.
Bridge removes the marker only when it ends the answer.

An optional question plugin observes completed main-loop `turn.step` responses and
retains structured `AskUserQuestion` emissions only for the original bound turn. It is
separate from the completion plugin, so a policy refusing question observation can leave
completion active; Claude 2.1.309 accepts one hooks module per plugin. Evidence comes only
from the final `next.trace` engine/core return with `stopReason: tool_use`. An absent or
short trace supplies no evidence. The module passes
every streamed chunk and the step's result through unchanged. The official step carries
the turn id; ordinary `tool.call` and classic hooks do not. Each emission's `step:N/tool:M`
id identifies the step and the call's original position within that response. These ids
are observation cursors, not an endpoint for answering a dialog.

The Claude-owned `claude-questions-<claim>.json` retains question text and option labels
within a 64 KiB budget. Incomplete or conflicting evidence preserves the prior prefix and
marks it incomplete. A repeated model step, a non-tool-use response carrying a question,
or a lower hook's rewrite also marks incompleteness. Hooks above Bridge can still change
or discard the response after observation, so this does not prove a dialog was displayed
or a tool executed. The helper has two 5-second attempts; a failure logs unavailable
observation and leaves ordinary tool execution intact. Stream overhead and dialog delay
have not been benchmarked. Publication verifies the current claim and immutable binding under
the lifecycle lock; reads use the selected historical receipt under the query's lock.
`result`, `status`, `inspect` and `wait --questions` use the same contracts as Codex emissions.
Nothing marks a question pending or answered, changes permissions, or releases the claim.
Other plugins, subagents, legacy sessions and unbound continuation turns supply no evidence.
See the [official module reference](https://code.claude.com/docs/en/plugins/mods/reference)
and [Claude question verification](verification/2026-10-23-claude-question-emissions.md).

A normal nonempty bound answer without that marker is retained as a candidate. It does not
release the claim or become a result. A later main-loop completion in the same conversation
can finish it only with that candidate, the still-current request and a valid nonempty answer
ending in the exact marker. Its result has `claude_mod_marked_continuation` provenance; both
official turn ids remain in private records. This covers an unmarked background interim followed
by a marked final. It does not establish provider-owned parentage: an unrelated turn repeating
the same marker can match, and a marked interim can still complete early.

An unmarked genuine final can remain pending indefinitely, including after a caller timeout.
`tabcli inspect <session>` explains that state; JSON includes `provider_completion` when its
reader lock and current claim allow the candidate to be read. This query does not retry input,
repair records or publish results. Inspect the terminal before closing or diagnosing the session.

An official interrupted, API-error, refused or empty final outcome fails the bound request.
Failure does not prove that no work or tools ran. Malformed or unmatched signals retain the
claim and write the latest rejection reason to `claude-mod-diagnostic.json` in the session
directory. A message delivered into an unrelated running turn without its own exact start
cannot be correlated. Combined queued input that differs from the exact addressed frame also
stays unbound. A retained claim prevents another tell; inspect the result and terminal, then
close it if you abandon it. On macOS, start a fresh `ask` with the needed context; native Windows
also supports `reopen`. These running-turn and combined-input limits follow from
exact matching and have not been driven live. Native completion wires are limited to 4 MiB of
UTF-8 JSON. Admission includes framing and escaping overhead; an oversized bound answer fails
without truncation. An unbound interruption, error, refusal, empty answer or oversized signal
has no sufficient attribution and leaves the request pending.
See the [October 22 regression and fix record](verification/2026-10-22-windows-260-reverification.md)
and the [earlier owner verification](verification/2026-10-19-windows-249-claude-native-completion.md)
for observations and limits. The earlier build's marker-omission behavior is superseded here.
The [October 23 macOS record](verification/2026-10-23-macos-249-claude-completion.md)
covers a configured blocking Stop hook, disabled readiness and ordinary permissions on 2.1.309.

An absent completion-mode record or explicit `legacy_stop` remains readable, including its
limitation when another Stop hook continues the turn. Legacy pending requests can finish after
a binary upgrade, but new tells into those sessions are refused on both platforms. Inspect the
result and terminal, then close when appropriate. On macOS, start a fresh `ask` with the needed
context; on native Windows, close and reopen to use native completion.
Legacy `StopFailure` cannot identify a pending cross-session request and leaves its claim held.
Do not downgrade the binary or `AGENT_BRIDGE_EXECUTABLE` while native-mode Claude sessions
remain open; older binaries cannot interpret their completion protocol.

**Other Claude sessions on the same account can send work to your Bridge-launched session.**
With `--yolo`, that work runs with skipped permission checks. Do not use that combination if you
do not control the account's other Claude sessions. See
[Claude inbound messages](security-and-data.md#claude-inbound-messages).

You can `reopen` a closed Claude session on native Windows. Bridge creates a new session and
runs `claude --resume` with the recorded conversation UUID. It checks for other live holders
before launch and delivery, but Claude does not provide exclusive ownership: another session
can resume between checks. A conflicting follow-up is refused before delivery and leaves the
session ready. Close the other holder before trying again.

macOS reopen is refused because its ownership path is not verified live. Bridge copies no model,
effort, or bypass option from the closed source. Claude applies its own
[resume rules](https://code.claude.com/docs/en/sessions#what-a-resumed-session-restores)
and settings; omitting `--yolo` therefore does not prove that bypass is off.

`cancel` is unsupported in this release's Claude integration. The completion module observes
interruptions but implements no addressed interruption request. An observed interruption does
not identify who caused it. Starting a separate resumed process does not cancel the running TUI.
Support needs an official way to interrupt this running interactive session from outside it.

## Agy

Agy's submission evidence is its receipt; it does not use Codex's composer protocol.

Install and sign in to `agy` (Antigravity CLI) 1.1.12 or newer. Trust the exact project directory
in Agy before launching it through Bridge.

On macOS, Bridge passes the initial prompt with `--prompt-interactive`. Agy can run that prompt
behind its workspace-trust dialog, so receiving the first result does not prove it is ready for
a follow-up. Windows initial input and all follow-ups use terminal input because Agy has no
integrated first-party input API or accepted-turn signal.

The Windows initial paste and every macOS follow-up wait for startup completion and a quiet
period in the session's `agy.log`; the Windows initial paste also requires a full redraw. Windows
follow-ups take a fresh log offset before delivery rather than repeating those readiness checks.
Every paste requires a matching input receipt afterward.

The Windows initial paste and macOS follow-ups also require Agy's trust entry for the exact
workspace and a log entry showing that this session loaded the workspace customizations.
Trusting the directory in another Agy session does not establish that this session is ready.

If Bridge says the session's trust is unverified, no prompt was pasted. Look at the managed
surface: approve a trust dialog if one is there and you trust the directory. After resolving it,
retry a withheld macOS follow-up with `tell` if the session is `ready`. If the Windows initial
request failed, close that session and run a new `ask`. If the composer is already visible, or
input remains withheld, close the session and start a new one. A missing log entry alone does not
prove a dialog is open.

After input, Bridge requires a `HandleUserInput` receipt in the log carrying the turn marker.
A missing receipt is different from a withheld paste: the prompt could have arrived. The
session stays `working`, retains its claim, and records the reason in its status. Do not resend;
inspect the same request's result or explicitly close the session.

Bridge reads completed results from Agy's transcript and checks the claim and final marker.
It can also report the observed quota-exhaustion failure from the log when the log proves it
belongs to the pending turn. A result already written takes precedence over that failure.

In the default permission mode, Agy can ask to run a command even for a self-test reply
that needs no tools. This was observed once with Agy 1.3.0 on 2026-10-07: its log named
`RunCommand`, but did not identify the command. The self-test now explicitly says that no
tool, command, or file is needed; whether that prevents the prompt requires live testing.

`doctor SESSION` reports `agy_tool_confirmation_observed` when this session's own `agy.log`
binds a `Surfacing tool confirmation` line to the pending turn, with no result, receipt
failure, or executor error. It names the tool and says the turn is waiting for approval in
the terminal and Bridge does not answer it. This is the last observed confirmation, not
proof that the dialog remains open: no approval-resolution signal has been verified.
The observation neither fails the request nor releases its claim. A later result or executor
error follows the ordinary monitor rules. A self-test result timeout includes this diagnosis
in its step reason when `doctor --json` identifies the same request.

`reopen` refuses Agy sessions. Its presence records do not prove who still holds a conversation,
and Bridge's transcript reader follows only a newly created conversation. Start a new session.

`cancel` is unsupported in this release's Agy integration. The 1.3.3 investigation found
no documented cancel CLI/RPC for the running TUI. Neither an interruption log with turn
identity nor the `Stop` hook's behavior and reason on Esc was verified. Support needs an
official addressed turn-interruption path and a correlated interruption signal; the
observed quota-failure log is not evidence of cancellation.

## Pi

Install and sign in to `pi` 0.84.1 or newer. If Pi asks you to resolve project trust, answer that
prompt before launching through Bridge.

On macOS, the initial prompt is a launch argument. On native Windows, Bridge waits for Pi's
`session_start` event with reason `startup` for the current claim before typing. That event
follows the trust procedure, including a possible decline; it does not grant shared consent.
If the wait expires, the error says no initial console input was sent. Resolve the prompt in
the managed surface, then close and start a new Bridge session. Follow-ups use terminal input.

Bridge installs a session-local extension that checks the actual `before_agent_start` prompt
against the claim. It collects the result at `agent_end` and reports it at `agent_settled`, so an
accepted prompt can produce an exact result body without an added marker. The result includes
Pi's session and turn identity. Errors reported by those events are recorded too; a separate failure signal
preserves an error when the extension cannot deliver the result to Bridge.

On native Windows, both initial and follow-up input require the extension's
`input_ack_schema: 1` startup capability. Bridge refuses input before typing when the session's
extension did not report that capability and asks you to start a new session. Bridge sends one
console batch, then waits for that claim's `agent_start` receipt. The extension matches the
SHA-256 and UTF-8 byte length of the complete framed prompt against `pi-pending-turn.json`
before publishing `pi-input-accepted-<claim>.json`. It accepts both the older string-prompt
event and the newer explicit user event; custom runs cannot acknowledge input. This fallback
assumes no concurrent input. The match identifies the framed prompt, not who typed it;
a manual exact copy of the complete framed prompt is indistinguishable.

On native Windows, even with `--detach`, the command waits for this submission evidence and
can wait until the full original request deadline. A lost Return, input transformed by another
extension, a busy run without a fresh correlated start, or a receipt publication failure leaves
delivery uncertain. The sender re-reads the receipt once after observing claim retirement or
close. Publication after that re-read, or evidence first observed after the deadline, still
reports uncertain delivery even if Pi started the prompt. A result can still arrive. Inspect
the request and Pi editor before further input; never retry the prompt or Return automatically.
The receipt records historical submission, including after claim retirement or a Pi conversation
change. It grants no result, input, or cancellation authority and stays with the session's
retained records through close. Do not downgrade the Bridge binary or `AGENT_BRIDGE_EXECUTABLE`
while these sessions remain open. An older binary can drop the startup input
capability on a follow-up, causing later input to be refused. macOS keeps its existing input
paths. Replace this receipt fallback when Pi exposes addressed interactive input with per-turn
acceptance.

`cancel` supports sessions whose installed extension has reported schema 2 in
`pi-startup-ready.json`. Bridge carries that capability evidence to each new Claim when
installing its pending turn; the receipt must identify the current Claim. Schema 1,
missing, or unreadable readiness is unsupported: start a new session.

The extension uses Pi's official `ctx.abort()` API through an adapter-local fallback:
every 200 ms during a run it reads Bridge's `cancel.json`, checks the active Claim and
the correlated prompt, and calls abort at most once for that Claim. This is a polling
interval, not a response-time guarantee. `pi-cancel-accepted.json` records the call;
it is published with a temporary file and rename. A failed write never causes another
abort. The adapter removes acceptance when installing the next Claim or rolling back
its pending delivery. Acceptance alone never releases a Claim or creates a Result.

Abort is not a completion acknowledgement. Only an accepted call for the same Claim,
an `agent_end` whose last assistant message has either `stopReason: "aborted"` or
`stopReason: "error"` with exactly `errorMessage: "This operation was aborted"`, and
matching cancel intent produce a cancelled Result through the ordinary `agent_settled` hook.
The second form was observed during a tool abort on Pi 1.0.4 (reported live run,
2026-10-11 04:07 KST, session-CfVS05): Node's AbortError message reached the assistant's
`errorMessage`. This is an exact, version-evidenced message match, not a general error
or substring heuristic. Pi's [session abort][pi-session-abort] calls the agent's abort;
the [agent loop][pi-agent-loop] forwards the stream's final assistant message and ends
on either `error` or `aborted`, without normalizing that outcome. These source paths
confirm propagation, not the exact backend throw site in that live run.
Direct Esc without this evidence remains an ordinary failure; Bridge does not infer an
actor. Normal completion can win the race. Retry or compaction waits may settle without
a new `agent_end(aborted)`; the watcher stops at `agent_end`, and the Result then remains
what Pi reported. Process exit before the result hook keeps the existing exit, close,
and recovery semantics; the accepted record cannot confirm cancellation.

Pi restores queued messages into the editor when aborting. Bridge does not use Pi's
queue, so without input queued or left in the editor by the user, the editor is empty.
**Inspect the managed surface before the next `tell` and clear or submit any restored
input yourself.** Bridge does not delete editor contents or detect that they are empty;
a terminal paste can otherwise combine with that input.

This fallback follows Pi v1.0.4's [abort API][pi-abort], [context lifetime][pi-context],
and [timer lifecycle rules][pi-lifecycle]. Replace the polling and private acceptance
record when Pi provides an official endpoint addressing a running interactive session
for interruption. Automated extension tests do not verify authenticated abort, restored
editor contents, or subsequent delivery on macOS or Windows.

[pi-abort]: https://github.com/earendil-works/pi/blob/v1.0.4/packages/coding-agent/src/core/extensions/types.ts
[pi-context]: https://github.com/earendil-works/pi/blob/v1.0.4/packages/coding-agent/src/core/extensions/runner.ts
[pi-lifecycle]: https://github.com/earendil-works/pi/blob/v1.0.4/packages/coding-agent/docs/extensions.md
[pi-session-abort]: https://github.com/earendil-works/pi/blob/v1.0.4/packages/coding-agent/src/core/agent-session.ts#L2268
[pi-agent-loop]: https://github.com/earendil-works/pi/blob/v1.0.4/packages/agent/src/agent-loop.ts#L354

Pi documents `usage` as a required field of every assistant message (`docs/message-types.md`,
"Assistant messages always contain usage"; `Usage` in `@earendil-works/pi-ai`). The extension
sums that field over the assistant messages that every `agent_end` of the correlated claim
delivers, until the request settles, and reports the sums beside the result; `result --json`
shows them as `usage` with `usage_reason` and `usage_error` (see the CLI guide). Pi's agent loop
emits only the run's new messages at `agent_end` (v1.2.0, `packages/agent/src/agent-loop.ts`), so
retries and recovery runs of the same claim add up without deduplication, and `runs` counts
them. The scope is one request, assistant messages only: a tool result's own `usage` (nested
model work), compaction and branch summaries (published through other events, possibly before
the prompt is correlated) and Pi's computed `cost` are not recorded; cost is excluded by issue
#55, which rules out price-based estimation. `reasoning_tokens` is reported only when every
assistant message of the request carried `reasoning`, because it is a subset of `output` and a
partial sum would misrepresent it. Every operand must be a non-negative safe integer and every
sum must stay one; otherwise, or when an assistant message lacks `usage`, the extension sends a
bounded `usage_error` and no partial sums. `coverage` is `complete` only when the final assistant
message of every run has `stopReason: "stop"`; Pi emits a synthetic failure message with zero
usage after an escaped exception and earlier calls may be missing, so an aborted, failed or
retried run is `uncertain`. Usage is reported for successful, failed and cancelled outcomes alike
and never changes the outcome; a failure signal written when delivery fails keeps it too. Verified
live on 2026-10-17 with Pi 1.2.0 against the session JSONL (one request of one run with a tool
call: two assistant messages, 793 input, 58 output, 16640 cache read, 23 reasoning, 17491 total).
Replace the extension's accumulation when Pi reports a turn's usage through an addressed
first-party signal.

Pi 1.0.0 can reject a prompt for missing credentials before an agent turn starts, without
an extension event identifying that refusal. Bridge therefore leaves that request pending
and claimed. `doctor SESSION --probe` reports `pi_provider_credentials` using Pi's
`auth check --no-refresh --json`, without a model call or credential output. It first asks
Pi to resolve the recorded qualified model, then checks its provider with `--provider`;
unqualified names, unknown providers, or a different resolved provider remain `unknown`.
The two commands share a three-second budget within doctor's five-second probe budget.
`ready` means credentials are configured, not that they are valid or accepted. `not_ready`
means Pi reports no configured credentials. Missing commands, timeouts, and unreadable or
unsupported responses are `unknown`. Without a session model or `--probe`, no auth check
runs. A self-test result timeout includes this observation before cleanup; it does not
record a failed request, release its claim, or resend input.

`reopen` refuses Pi sessions because Bridge cannot verify whether a live process still holds
the conversation. Start a new session instead.

## Verification boundary

The delivery paths above are implemented for macOS and native Windows. Automated adapter and
terminal tests cover transport choices, correlation, and refusals; they do not authenticate to
model services. This table lists authenticated live evidence and its limits. Unlisted
provider/terminal combinations are not verified.

| Provider/platform | Live evidence | Limitations |
| --- | --- | --- |
| Codex/macOS | [2026-10-15 published 0.4.9][mac49], [2026-10-11 candidate 0.4.3][mac43] | codex-cli 0.162.1; iTerm2 and WezTerm self-tests respectively, defaults; installed 0.4.9 also completed two Ghostty 1.3.1 `ask` round trips; no Ghostty cold start (#104) |
| Claude/macOS | [2026-10-15 published 0.4.9][mac49], [2026-10-11 candidate 0.4.3][mac43] | Claude Code 2.1.300/iTerm2 and 2.1.296/WezTerm respectively, defaults; no reopen |
| Agy/macOS | [2026-10-15 published 0.4.9][mac49], [2026-10-11 candidate 0.4.3][mac43] | Agy 1.3.5/iTerm2 and 1.3.3/WezTerm respectively; `--yolo` |
| Pi/macOS | [2026-10-15 published 0.4.9][mac49], [2026-10-11 candidate 0.4.3][mac43] | Pi 1.2.0/iTerm2 with `--model openai-codex/gpt-5.6-sol`; Pi 1.0.4/WezTerm with defaults |
| Codex/Windows | [2026-10-12 candidate 0.4.9][win49], [0.4.8 checks][win48] | 0.4.9 self-test uses `--terminal windows-console --yolo`; record identifies Codex CLI 0.162 through npm; 0.4.8 record identifies 0.162.1 |
| Claude/Windows | [2026-10-12 candidate 0.4.9][win49], [0.4.8 checks][win48], [reopen][old7] | 0.4.9 record does not give a Claude version; 0.4.8 used 2.1.296; reopen evidence dates to 2026-09-24 |
| Agy/Windows | [2026-10-02][win10] | Older authenticated evidence; not run in the 0.4.8 or 0.4.9 records |
| Pi/Windows | [2026-10-02][win10] | Older authenticated evidence; not run in the 0.4.8 or 0.4.9 records |
| All/Linux | Not verified | Refusal tests only; no visible-session transport |

The latest Mac record covers all four providers on iTerm2 with the downloaded, published 0.4.9
binary, after publication; it did not gate the release. Codex and Claude used defaults, Pi selected
`openai-codex/gpt-5.6-sol`, and Agy used `--yolo`. The 0.4.3 candidate record covers all four in
WezTerm, including Pi 1.0.4, with defaults for Codex, Claude and Pi and `--yolo` for Agy.
Terminal.app, WezTerm and Warp were not verified on macOS 0.4.9; the latest record's two Ghostty
`ask` round trips used the installed 0.4.9 with Ghostty already running, not a cold start (#104).
The Windows 0.4.9 record covers candidate Codex and Claude self-tests, not a downloaded published
binary. The 0.4.8 record measures Codex newline preservation (with the emoji loss of #147, since
fixed) and environment
scrubbing on both Windows surfaces; the Windows Terminal cold-start observation of #111 remains
open. The [daemon-absent Codex queue probe][queue] used a mock model, not an authenticated TUI.

Warp requires a reachable official Control endpoint and provable surface ownership. It cannot
submit terminal input, so Agy and Pi follow-ups are unsupported there; choose another terminal
for those providers. Warp has automated tests. The [0.1.1 check][mac11] returned an empty Warp
instance list; Warp was not exercised for [0.1.2][mac12] or the latest Mac run (#63, #87).
Linux is unsupported even where shared code compiles. See [terminal support](terminals.md) for surface-specific constraints.

[mac12]: verification/2026-10-06-macos-0.1.2.md
[mac11]: verification/2026-10-04-macos-0.1.1.md
[win10]: verification/2026-10-02-windows.md
[old7]: releases/0.0.7.md
[queue]: verification/2026-10-04-codex-queue.md
[mac43]: verification/2026-10-11-macos-0.4.3.md
[mac49]: verification/2026-10-15-macos-0.4.9.md
[win48]: verification/2026-10-12-windows-0.4.8.md
[win49]: verification/2026-10-12-windows-0.4.9.md
