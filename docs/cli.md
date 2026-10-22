# Command reference

Terminal Agent Bridge (TAB) is installed as `tabcli`.

Use `ask` to start a provider session, `result` to read its result, `tell` to continue it, and
`close-session` to close it when you are done. These commands leave the provider visible in a
terminal surface: the tab, window, pane, or console that Bridge creates for the session.

Start in a project directory with an installed, authenticated provider. This example uses Codex with
its defaults. Replace `/path/to/project` with your directory, then use the ids printed by your own
run in the later commands. The ids and results shown here are invented; JSON output is trimmed to
the fields being discussed.

```sh
tabcli ask codex --workspace /path/to/project --prompt "Where does this program start?"
```

```text
session: session-K7m2Qx
request: request-1791285000000000000-4217-0

The program starts in src/main.rs.
```

The session is the provider conversation. The request identifies this prompt and its result. Read
that same result again without making another model call:

```sh
tabcli result session-K7m2Qx --request request-1791285000000000000-4217-0 --json
```

```json
{
  "ok": true,
  "session": "session-K7m2Qx",
  "request_id": "request-1791285000000000000-4217-0",
  "request_state": "completed",
  "result": "The program starts in src/main.rs."
}
```

The session stays open after a result. Send a follow-up to continue the same conversation; Bridge
gives it a new request id.

```sh
tabcli tell session-K7m2Qx --prompt "Which function parses its arguments?"
```

```text
session: session-K7m2Qx
request: request-1791285060000000000-4281-0

parse_args_from parses the command-line arguments.
```

Close the surface when you have finished. The recorded results remain available afterward.

```sh
tabcli close-session session-K7m2Qx --explicit
```

```text
closed session-K7m2Qx
```

For a timeout or delivery error, use the
[shared recovery instructions](#shared-prompt-and-output-options) before sending another prompt.

The synopses below use `PROVIDER` for `codex`, `claude`, `agy`, or `pi`. `SESSION` and `REQUEST` are
Bridge ids, not the provider's conversation or turn ids. See [providers](providers.md) for
installation requirements and [terminals](terminals.md) for terminal selection.

Run `tabcli --help` (or `-h`) for the main help and `tabcli --version` (or `-V`) for the
installed version; these flags take no further arguments. A public command followed immediately by
`--help` also shows the main help.

## ask

Start a new provider conversation when you do not want to continue an existing session.

```text
tabcli ask PROVIDER [--workspace PATH] (--prompt TEXT | --prompt-file PATH)
    [--title NAME] [--model MODEL] [--effort EFFORT] [--terminal TERMINAL]
    [--yolo] [--timeout-secs N] [--detach] [--json]
    [--context-result SESSION/REQUEST]... [--context-mode <full|compact>]
```

```sh
tabcli ask codex --prompt "Summarize this project." --detach --json
```

```json
{
  "ok": true,
  "session": "session-N8v4Lc",
  "request_id": "request-1791285120000000000-4310-0",
  "request_state": "accepted",
  "result": null
}
```

The detached example returns the session and request ids without waiting for a result. To wait in
the same command, omit `--detach`.

`--workspace` defaults to the current directory and must resolve to an existing directory. `--title`
defaults to `<provider> · <workspace-basename>` (or `workspace` when the basename is unavailable).
Titles keep at most 80 characters, replace controls with spaces, collapse whitespace, and must
remain nonempty. They are record labels; a terminal can also use its own ownership title.

`--terminal` defaults to platform detection. Documented values are `ghostty`, `iterm2`, `terminal`,
`warp`, `wezterm`, and `windows-console`; only platform-supported values can launch. See [terminal
selection](terminals.md#selection-and-settings), including accepted aliases.

`--model` and `--effort` default to no Bridge override and must be nonempty when provided. Provider
defaults apply. Effort is forwarded to Codex's reasoning configuration, Claude and Agy's effort
option, or Pi's thinking option; Bridge does not enumerate allowed effort values. Model strings are
forwarded with provider-specific normalization described in
[providers](providers.md). These options configure the new session only.

`--yolo` defaults to off and is never inherited as a Bridge option. When explicitly supplied, Codex,
Claude, and Agy receive their native bypass options; Pi receives its project-local trust approval
option, while its native tool policy remains in force. Provider settings and environment still
matter. This option is not a universal way to dismiss workspace trust dialogs.

The remaining options use the [shared prompt and output rules](#shared-prompt-and-output-options).
Human output prints `session:`, `request:`, then the result when waiting completes. A detached JSON
success has `request_state: "accepted"` and a null `result`; a waited success has `request_state:
"completed"`. See the [JSON field reference](#json-field-reference).
After arguments have been parsed successfully, with `--json`, failures before a request address exists
print an error object containing only `ok: false`, `error`, and `schema_version`; later failures include
the request address.

Without `--detach`, exit 0 means Bridge received a successful result. With it, exit 0 means the
launch and delivery steps succeeded; retrieve the result using the returned request id.

Launch admission runs once the workspace is resolved and before the provider CLI is checked or any
session record, consent record, or terminal surface exists. With the default setting
`codex-memory-workspace refuse`, Bridge refuses a workspace that is the Codex memory workspace or lies
inside it, whatever the target provider:

```text
launch refused: workspace <workspace> is within the Codex memory workspace <memory>; codex-memory-workspace is refuse. To allow launches here, run 'tabcli settings codex-memory-workspace allow' in the same state root.
```

The memory workspace is `memories` beside Codex's `config.toml`: `$CODEX_HOME/memories`, or
`.codex/memories` under `HOME` (then `USERPROFILE`) when `CODEX_HOME` is unset. It is read from the
caller's environment, exactly as workspace trust reads it: a relative `CODEX_HOME` is relative to the
caller's working directory, not to `--workspace`; `~` is not expanded; an empty value is not
rejected; and with neither `HOME` nor `USERPROFILE` the launch fails even when `CODEX_HOME` is set.
Both paths are compared after resolving links, by whole path components, so a sibling such as
`memories-other` is outside. A memory workspace that does not exist protects nothing; one that cannot
be resolved, or that is not a directory, fails the launch. The rule exists because Codex's
background memory consolidation started Bridge sessions there that nobody closed; it also refuses a
launch you start there yourself. Run `tabcli settings codex-memory-workspace allow` in the same
state root to permit it. The refusal uses the error object above; no session or request id exists.

If the workspace cannot be resolved, check `--workspace` and use an existing directory. For a
missing or too-old CLI, run `tabcli doctor --provider PROVIDER --probe`, then correct the
installation it reports. An Automation error calls for the
[macOS permission checks](macos-permissions.md). If trust is unverified, look at the managed
surface and answer a workspace-trust dialog only if you intend to trust that directory. For Agy, if
the surface shows the composer instead of a trust dialog, or input remains withheld after approval,
close the session and start a new one. Do not resend after uncertain delivery. For other providers,
inspect the session if no dialog is present.

For timeout or delivery errors, follow the
[shared recovery instructions](#shared-prompt-and-output-options).

## tell

Use `tell` for the next prompt in a session whose previous request has finished.

```text
tabcli tell SESSION (--prompt TEXT | --prompt-file PATH)
    [--timeout-secs N] [--detach] [--json] [--context-result SESSION/REQUEST]... [--context-mode <full|compact>]
```

See the command and output in the [opening workflow](#command-reference).

Bridge checks the hold before changing any target records, then finishes interrupted record
updates and verifies that it still controls the surface. The session must be `ready` and not held;
an unfinished request blocks another follow-up. A hold set after delivery began does not stop that
turn. All listed options follow the shared rules; the timeout defaults to 900 seconds. There are no workspace, model,
effort, title, terminal, or bypass options for a follow-up.

Human and JSON output, including detached `accepted` and waited `completed`, match `ask`.
After arguments have been parsed successfully, with `--json`, failures before a request address exists
also include the target `session` in the error object; later failures include the request address.

Codex uses its addressed native queue and does not require a shared daemon. There is no Codex
terminal-input fallback. Claude uses official cross-session messaging, including a messenger model
turn; unavailable messaging is an error, not a terminal fallback. Agy and Pi use their adapter-owned
terminal fallback. See [providers](providers.md) for readiness and receipt gates.
On native Windows, Codex follow-ups also have a measured command-line size refusal; see the
[Codex send-side limit](providers.md#codex) for the limit and remedy.
On native Windows, Pi waits for its extension's per-turn submission receipt even with `--detach`.
That wait shares the request deadline. Bridge refuses input before typing when the session's
extension did not report the required capability. See
[Pi](providers.md#pi) for uncertain-delivery recovery and receipt limits.

Exit behaviour matches `ask`. If you see `tell requires the ready state`, inspect the session and
wait for its active request with `result SESSION --request REQUEST --wait`. A closed session cannot
accept `tell`; use `reopen` where supported or start a new session.

For an ownership error, run `doctor SESSION` and check the managed surface. Bridge refuses input
when it cannot prove that it controls that surface. For `delivery uncertain`, inspect the request
named in the error. Bridge has not proved whether the provider received it and keeps the claim to
block duplicate delivery. Wait for that request or close the session; do not repeat `tell`.

## hold

Record or release the user's request to refuse further follow-ups.

```text
tabcli hold SESSION [--release] [--json]
```

A hold refuses a follow-up whose delivery Bridge has not yet allowed to start. It does not stop
an already allowed delivery, the current turn, the initial prompt, or direct input to the surface.
`hold SESSION --release` allows later follow-ups to pass the hold check; they still require `ready`.
An unreadable hold record also refuses admission until its intent can be read.

The command requires a valid session manifest. Set and release are idempotent in intent:
`changed` reports a change of hold intent, not an absence of filesystem writes or waiting.
An already held set preserves the timestamp. A malformed record is normalized by set, or removed
by release, with `previous: "malformed"` and `changed: true`. Filesystem read failures are errors.
Both operations refuse a closed session. Close preserves its hold record; prune removes it with
the session directory. Reopen creates a separate session and does not inherit the hold.

Human output is one line. JSON success has `schema_version`, `ok`, `session`, `held`, and
`changed`, plus `previous` when normalizing malformed intent. Errors exit nonzero; with `--json`,
argument errors also produce a structured error on stdout. A Hold refusal in `tell` before a claim is
returned has `{schema_version:1, ok:false, session, error}` and no request id. A receipt already
created during a concurrent admission refusal remains unresolved and can be found with
`result SESSION --list --json`; no request identity is invented for the error.

## cancel

Use `cancel` to request interruption of the session's active Request. This release supports
Pi only, when its installed extension has reported schema 2 startup readiness bound to the
current Claim; older sessions must be replaced.
See [provider support](providers.md) for the supported integrations and session requirements.

```text
tabcli cancel SESSION [--json]
```

The session must be `running` or `working`, with an active Claim and its Receipt, no
published Result for that Request, and cancel support verified for that session. A Receipt
exists before dispatch; recording a cancel does not prove the prompt has reached the provider.
The provider integration must confirm that the addressed prompt is running before interrupting it.

Success records intent and returns `state: "requested"`, the `request_id`, and
`created_unix_ms`. It does not confirm interruption. Use `result SESSION --request REQUEST --wait`
to await the outcome: only a correlated provider interruption produces `request_state: "cancelled"`.
A normal completion can win the race. Cancel leaves the session, Hold, and follow-up delivery
unchanged; a repeated cancel for the same Claim is refused.

`inspect` shows the retained request as `requested`, `applied`, `not_applied`, or `unreadable`.
The record survives completion and the next Claim; a later cancel replaces it, so its earlier
intent is no longer visible. A damaged cancel record does not hide a published Result.

## reopen

Use `reopen` to continue a closed Claude conversation on native Windows in a new Bridge session.

```text
tabcli reopen SESSION (--prompt TEXT | --prompt-file PATH) [--title NAME]
    [--model MODEL] [--effort EFFORT] [--terminal windows-console] [--yolo]
    [--timeout-secs N] [--detach] [--json]
```

```powershell
tabcli reopen session-C4r8Ta --prompt "Continue the review." --json
```

```json
{
  "ok": true,
  "session": "session-P9c2Fr",
  "request_id": "request-1791285240000000000-4400-0",
  "source_session": "session-C4r8Ta",
  "resumed_from": {
    "session": "session-C4r8Ta",
    "provider_session_id": "de6896ba-1b03-4d72-8073-ecab40596c3e",
    "event_id": "event-1791285000000.json"
  }
}
```

This command supports only Claude Code on native Windows; the example assumes the source is a closed
Claude session. The source must be closed, have a recorded Claude conversation id, and have readable
results for its recorded requests. The workspace is reused from the source and must still exist.
Bridge saves a marker on the source to allow only one reopen from it. Workspace consent is
assessed for the new session exactly as `ask` assesses it, from the current trust stores and
Bridge's shared consent records, never from the source session's records; with verified consent
and without `--yolo`, a reopened session can take the same guarded trust-dialog answer as a fresh
`ask`. The assessment and its record precede the source marker, so a failure in either leaves
the source reopenable.

Options follow `ask`, with a 900-second default timeout. The default title is `<provider> ·
<workspace-basename> (reopened <source-session>)`. Model, effort, and bypass options are not copied
from the source manifest: Bridge forwards only explicit overrides. Claude can restore its own
conversation settings, so omitting `--yolo` does not establish that bypass is off. There is no
`--workspace` or `--context-result` option.

Human output adds `source_session:`. JSON adds `source_session` and `resumed_from` to the `ask`
response; `resumed_from` contains `session`, `provider_session_id`, and `event_id`.
After arguments have been parsed successfully, with `--json`, failures before a request address exists
print an error object with `source_session`, `session: null`, `request_id: null`, and `gate` (null when no
refusal gate applies).
Later failures include the request address, `source_session`, and `gate`.

Exit behaviour otherwise matches `ask`. Refusal gates include `provider-unsupported`,
`workspace-refused`, `start-failed`, `launch-failed`, `reopen-conflict`, `reopen-verification-failed`, and
`already-reopened`. Source refusals include
`source-not-closed`, `source-not-converged`, `request-unresolved`, and `source-identity-missing`.
For `source-not-closed`, close the source first. For `source-not-converged`, repeat `close-session
SESSION --explicit` on the source, then inspect it again. `request-unresolved` and
`source-identity-missing` mean Bridge cannot verify the recorded results or find a conversation to
resume; inspect the source and use a new `ask` if that evidence is unavailable. A request that this
release settled as `Delivery::NotSent` is a recorded `failed` request (`delivery_not_sent`) and does
not block reopen. A receipt without any event does: one recorded by an earlier release, or a
reopened-conversation follow-up refusal, which still leaves such a receipt.

`workspace-refused` is the launch admission of [`ask`](#ask) applied to the source's workspace. It
runs after the platform and provider check, so `provider-unsupported` still comes first, and before
the new session exists: the source's marker is not taken and no refusal record is written. Permit
the workspace with `tabcli settings codex-memory-workspace allow` in the same state root, or start a
new `ask` elsewhere.

`start-failed` means the new session existed but a record it needs before its launch (its result
context or its initial claim) could not be made. No provider process exists at that point, so the
new session is left failed with the refusal recorded, the source marker is released by the same
command, or by the next `reopen` if the command did not get that far, and a new `reopen` of the
source is admitted.

`launch-failed` means the launcher failed after the initial request was recorded. Bridge
releases the source marker only with a matching launch receipt and proof that no provider
spawn remains unresolved, or that the recorded provider process is verified gone. Live or
unverifiable processes and missing, damaged or mismatched records retain the marker. A
failure before the launch receipt is durable therefore retains it. A later `reopen` retries
an interrupted failure fence before checking the marker; observation commands do not repair
it. The original launch error remains in the response.

For `provider-unsupported`, check the platform and provider before retrying. For `reopen-conflict`,
stop the other resume yourself before trying another delivery. Run `doctor SESSION` for
`reopen-verification-failed` or `already-reopened`; it reports other live holders and retained
markers. Do not delete a marker to force a second resume.

Bridge checks for other live holders before starting Claude, after registration, before initial
delivery, and before each follow-up. These checks do not reserve the conversation; another Claude
resume can start between them.

After refusing a launch, Bridge releases the source marker only when it proves the refused launch
cannot still hold the conversation. Otherwise it keeps the marker. A later `reopen` checks the
recorded refusal again and can release the marker once the provider process is verified gone.
`doctor` reports this condition but never releases the marker. See [architecture](architecture.md).

## sessions

Run `sessions` to find a session id or list sessions left open by earlier commands.

```text
tabcli sessions [--workspace PATH] [--provider PROVIDER] [--state STATE]
    [--sort id|updated] [--json]
```

```sh
tabcli sessions --workspace /path/to/project --state ready --json
```

```json
[
  {
    "id": "session-K7m2Qx",
    "provider": "codex",
    "state": "ready",
    "results": 2
  }
]
```

List all recorded sessions by default, including closed ones. Workspace and provider filters are
absent by default. Workspace matching uses a canonical path when resolvable, otherwise an absolute
path. `--state` is an exact string filter, not a validated enumeration. `--sort id` (the default)
sorts ascending by id; `updated` sorts newest first, breaking ties by id.

**The public command is not read-only.** For sessions matching workspace and provider, it attempts
publication of recorded completions, completion of interrupted closes, and repair of sessions whose
owner died before reading their state. The state filter is applied afterward. The internal read-only
listing is not a public flag. Use `status`, `inspect`, `result`, or `search` when records must not change.
Each row also carries `origin` and `origin_observation`, read the same way as in `status`. That
reading only observes: it runs after the repair above and changes no record.

Human rows show id, state, provider, workspace, terminal, bypass flag, and recorded event count. An
empty list prints `no native Agent Bridge sessions`. JSON is an array, not an `ok` envelope. The
`results` count includes recorded events, not only successful published results. See the [JSON field
reference](#json-field-reference).

An empty list exits successfully. If a session you expect is missing, check the workspace, provider,
and state filters, then the selected state root. Unreadable manifests are skipped. Individual repair
errors do not always fail the command; use `inspect SESSION` or `doctor SESSION` to investigate a
session that stays in an unexpected state. Root enumeration and other command-level I/O errors exit
nonzero; check the path named in the error.

## status

Use `status` to list session observations without changing records.

```text
tabcli status [--workspace PATH | --all-workspaces] [--provider PROVIDER] [--all] [--json]
```

```sh
tabcli status --workspace /path/to/project --json
```

```json
{
  "schema_version": 1,
  "ok": true,
  "filters": {
    "workspace": "/path/to/project",
    "all_workspaces": false,
    "provider": null,
    "include_closed": false
  },
  "sessions": [],
  "incomplete": false,
  "incomplete_reasons": [],
  "scanned": {"sessions": 0, "listed": 0}
}
```

The default scope is the canonical current workspace, as in `search`. `--workspace` overrides
it; `--all-workspaces` removes it; they are mutually exclusive. Provider defaults to all
providers. Workspace and provider filters are applied before reading session state or checking
the owner process.
Closed sessions are excluded by default; `--all` includes them. Failed and exited sessions are
included. The closed filter uses the recorded status without changing it.
The default output therefore does not show closed sessions or their residual surfaces.
Unlike `sessions`, this command never repairs, recovers, or writes records.

The ten-second budget is checked between operations, not a hard command deadline. Each busy
session read retries for at most 250 ms or the remaining budget, whichever is shorter.
Slow filesystem and process observations can outlast the budget. Each remaining session gets
an incomplete reason when the budget is exhausted. Session state is read consistently; owner
and terminal records are observed separately. The owner process is checked, but the terminal
surface is not probed. At most two result events are decoded per listed session: the latest
published event and the active request's published event when they differ. Result bodies are
not returned.

For Codex, status also reads at most one optional 64 KiB question record per listed session,
under its existing budget. Attention `question_emitted` means that record has emissions for
the active request, or for the request behind the latest published result when no claim exists.
A claim without a readable receipt has no question focus. Closed sessions have no
`question_emitted` attention. Changing focus does not resolve an older emission.
`question_observation` is present only as `incomplete` or `unreadable`; unreadable evidence
also adds `records_partially_unreadable`. Question content is in `result --request`.

A required-record failure omits that session and adds its id and reason to `incomplete_reasons`.
Unreadable hold, owner or terminal records, reopen history, latest events, or request indexes leave
the session in the list with attention flags. Fields whose evidence could not be read stay
null. Attention does not change session or request state. Owner exit, identity mismatch, and
missing observations are distinct facts; closed sessions have no owner liveness flags. A live
owner whose identity was not observed is not flagged; a failed identity check is reported as
`owner_unverified`. A bound owner whose attestation could not decide is `owner_unverified` only,
never `owner_exited`, even when its process id looked absent.
Each session carries `origin` and `origin_observation`: the managed session recorded as the
source of its initial request, and whether that session's records show it ended (see
[origin observation](#origin-observation)). A session whose recorded state is not `closed` and
whose origin has ended gets attention `origin_ended`; failed and exited sessions are included until
they are closed. The flag does not mean that a surface is open or that anyone abandoned the
session, and it grants no close authority. An origin that could not be observed adds no flag and
fails nothing; the reason is in the object. One status response reads each origin once.
`scanned.sessions` counts session reads attempted after the workspace and provider filters,
including failed reads and sessions excluded by the closed filter. `scanned.listed` counts
returned entries.

Entries with attention sort first, then by updated time descending, then by id ascending.
Human output gives an attention marker, id, provider, state, active request state, latest result
request state, and comma-separated attention flags. Missing request states show `-`. A footer
gives listed/scanned counts and every incomplete reason. An empty list prints `no sessions to show`
and exits 0. Partial observations also exit 0; argument and root-level read errors exit nonzero.
With `--json`, argument errors also produce a structured error on stdout.

`result_command` addresses a readable active receipt with `--request`, a claim without a
readable receipt with `--list`, or otherwise the latest published result with `--event`.
It is null when nothing is addressable. Active elapsed time is receipt-to-result time, as in
`result`; without a published result it is null with `no_published_result`.
See the [JSON field reference](#json-field-reference).

## inspect

Use `inspect` after a timeout or delivery error to read what Bridge recorded without changing it.

```text
tabcli inspect SESSION [--timeline [--request REQUEST]] [--json]
```

```sh
tabcli inspect session-K7m2Qx --json
```

```json
{
  "ok": true,
  "session": "session-K7m2Qx",
  "stored_state": "working",
  "turn_claimed": true,
  "recovery_required": false,
  "recorded_events": 1
}
```

Read a session without repair, recovery, delivery, close, or writes, in any recorded state. By
default, show stored state, owner observations, configuration, and request references. `--timeline`
shows recorded evidence. `--request` requires `--timeline` and filters request evidence without
removing session diagnostics.

Human output shows session, provider, workspace, stored state, hold, owner observations, errors, latest
result id, and request ids. In JSON, check `turn_claimed` for an active request and
`recovery_required` for a completion journal that still needs recovery. See the [JSON field
reference](#json-field-reference).

The default inspection also reports `held` as true, false, or null with `hold_error` when unreadable.
Timeline does not include hold history. The default inspection adds `origin` and
`origin_observation` as in `status`; the timeline has no origin entry.

For Codex, ordinary JSON inspection adds `question_emitted: true` to a request reference
when retained emissions exist. It adds `question_observation` only as `incomplete` or
`unreadable`. This reads at most one optional 64 KiB record per request. Retrieve content
with `result SESSION --request REQUEST --json`. Timeline output does not include emissions.

Timeline output groups request summaries and recorded evidence. Derived request summaries carry
`derived_from: "result"`; session diagnostics remain separate from request entries.

The timeline reads bounded, strict UTF-8 evidence, reports unreadable records, and leaves unknown
times unknown. A record changing during the read causes failure instead of an invented sequence.
Exit 0 means the read succeeded, even if the session has an error or the timeline is incomplete. For
a missing session, check the id and state-root override. If records changed during the read, repeat
the query; it sends nothing. For unreadable records, use `doctor SESSION` and keep the records for
diagnosis. An `incomplete` timeline reports the evidence it could read; it does not fill the gaps
with inferred events.

## result

Use `result` to retrieve a recorded result or wait for one particular request to finish.

```text
tabcli result SESSION [--latest | --list | --event EVENT | --request REQUEST] [--json]
tabcli result SESSION --request REQUEST --wait [--timeout-secs N] [--json]
```

```sh
tabcli result session-K7m2Qx --request request-1791285000000000000-4217-0 --wait --json
```

```json
{
  "ok": true,
  "session": "session-K7m2Qx",
  "request_id": "request-1791285000000000000-4217-0",
  "request_state": "completed",
  "result": "The program starts in src/main.rs.",
  "bridge_observed_elapsed_ms": 12438,
  "bridge_observed_elapsed_reason": null,
  "usage": null,
  "usage_reason": "not_reported",
  "usage_error": null
}
```

Read published results without changing records. Exactly one selector is allowed; `--latest` is the
default. `--list` prints event rows. With `--json`, it also includes a `requests` array, including
requests without an event; neither array includes result bodies. `--event` reads an event directly,
including older events with no receipt. `--request` follows the receipt of that exact request.
`--wait` defaults to off and requires `--request`; waiting for an unspecified latest turn is
refused. `--timeout-secs` requires `--wait` and defaults to 900 seconds.

An ordinary single Codex result can include `question_emissions` while the request is still
`pending`, and retains them after completion or close. Each item has the provider's `id`
and `questions`, whose `title` and optional `options` preserve the provider's strings.
Free-text questions use `options: null`. For example:

```json
{"question_emissions": [{"id": "call_example", "questions": [
  {"title": "Which report format?", "options": ["Text", "Table"]}
]}]}
```

Only a request that Bridge bound through its injected Codex hooks can have this evidence.
An unbound request settled through notify cannot have it.

These fields report emission, without changing request state, result text, or exit status.
`question_observation: "incomplete"` means Bridge retained a limited prefix or rejected
optional evidence. `"unreadable"` means the retained record could not be validated.
Absence of either field is unknown; it does not prove that no question existed.
After the session becomes `ready`, `tell` can carry a known fact as another request. It
does not resolve the earlier emission. `ask`, `tell`, `wait`, `result --wait`, `result --list`,
and timeline retain their output and waiting behavior. Full context attachments are unchanged;
compact context attachments also report `Questions: N emitted` when retained emissions exist. Bridge has no
transport that answers a question, and an emission does not end `wait` or `result --wait` early.

Human output gives session, request when available, request state, Bridge-observed elapsed time,
one usage line when the provider reported usage, each emitted question title and its options
with control characters escaped, result text, and error. List rows show event id,
request state, request id or `legacy`, and elapsed time. See the
[JSON field reference](#json-field-reference).

Elapsed time measures receipt creation to the published event, as Bridge observed them. It is
neither model nor billing time. An uncomputable value is null with a reason.

Token usage is collected only from a signal the provider itself documents as usage; today that is
Pi (every assistant message carries `usage`), tokens only. `usage` is null or an object with
`source` (`pi_extension_agent_end`), `scope` (`request`), `coverage`, `runs`, `assistant_messages`,
`input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens`, optional
`reasoning_tokens` (present only when every assistant message of the request reported it) and
`total_tokens`. The numbers are the sums over the assistant messages of every run of that request
until it settled, as Pi reported them; nested tool usage, compaction summaries and Pi's computed
cost are not included. `coverage` is `complete` when every run ended with an ordinary stop and
`uncertain` otherwise (an aborted, failed or retried run can leave calls unreported). `usage_reason`
is null with usage, else `not_reported` (the event carries no usage fields: legacy events and
providers that report none), `malformed` (the writer recorded a bounded `usage_error` naming the
field), `omitted_size` (the event exceeded the size limit with usage and was recorded without it; an
event that fits only without any usage field is recorded bare and reads `not_reported`) or
`no_published_result`. `usage_error` carries that bounded text and is otherwise null. Usage never
changes the request state. The same three fields appear in `--wait` replies, in both `--list`
arrays and in the timeline's request summaries. No cost is estimated or recorded.

Without `--wait`, a readable `--request` can exit 0 while reporting `pending`, `failed`,
`unresolved`, or `recovery_required`. A recorded failed event can also be read successfully. An
empty latest/event selection fails. With `--wait`, only `completed` succeeds; a terminal failure,
unresolved request, required recovery, dead owner, or timeout ends the wait unsuccessfully. If the
wait times out, query the same request again; do not send its prompt again. The request has not been
cancelled. `--list` can succeed with no entries. A missing receipt means the request cannot be
addressed that way; use event selection for legacy records. `recovery_required` means observation
cannot publish the completion; `sessions` is the public command that can finish those updates. Run
it for the session's workspace, then query the request again.

## wait

Wait for the first observed wait-ending condition among explicit request addresses.

```text
tabcli wait ADDRESS [ADDRESS ...] [--timeout-secs N] [--json]
```

```sh
tabcli wait session-K7m2Qx/request-1791285000000000000-4217-0 session-P9n3Rs/request-1791285000000000000-4218-0 --json
```

Each address must be `SESSION/REQUEST`, with a Bridge request id. At least one address is
required. Event names, `latest`, duplicate addresses, and paths are refused. Syntax and session
directories are checked up front; receipts are validated when that session first yields a
consistent observation. A busy session remains unconfirmed and does not block another session's
completion. An invalid address found in a pass fails the command before any successful selection;
later record deletion or damage is an error, not a pending request.

The wait ends on `completed`, `failed`, `cancelled`, `unresolved`, or `recovery_required`, or when a recorded
owner is no longer live or a closed, failed, or exited session leaves the request unresolved.
An owner is no longer live only when a bound owner's process exited or its process id now belongs
to another process, or an unbound legacy owner's process id is gone. A live owner with a changed
TTY or process group, an attestation that could not decide, and an owner record of another
session keep the wait going; `result --wait` follows the same rule.
As with `result --wait`, only `completed` exits successfully. If more than one address ends in
the same pass, the original input order wins, even across session groups. Sessions are observed
sequentially: there is **no global ordering guarantee** about actual completion times.

JSON contains `ended` (the selected result plus its `address`), `remaining`, and `timed_out`.
`remaining` means addresses not selected in this answer; they may be busy, unconfirmed, or already
ended. Human output has the same result lines as `result`, followed by one `remaining:` line.
See the [JSON field reference](#wait-1).

The shared timeout defaults to 900 seconds. At timeout, `ok` is false, `timed_out` is true,
`ended` is null, and all addresses remain. The command exits nonzero with
`waiting timed out; no request was cancelled or resent`. The deadline bounds polling and sleep;
it cannot interrupt an ongoing filesystem or owner observation. Each pass reads one consistent
snapshot per distinct session and releases its lock before reading the next session.

This command is read-only. It never cancels, repairs, closes, or resends any request. After a
timeout or uncertain delivery, inspect or wait on the same request; do not repeat its prompt.

## search

Use `search` when you remember text from a result but not its session or request id.
Cancelled results are excluded, like other failed results.

```text
tabcli search QUERY [--workspace PATH | --all-workspaces] [--provider PROVIDER]
    [--limit N] [--json]
```

```sh
tabcli search "entry point" --workspace /path/to/project --json
```

```json
{
  "ok": true,
  "query": "entry point",
  "limit": 20,
  "hits": [],
  "truncated": false,
  "incomplete": false,
  "incomplete_reasons": [],
  "scanned": {
    "sessions": 2,
    "events": 3
  }
}
```

Search published successful result bodies for a nonempty, case-insensitive substring. Failed events
are excluded; use `result` to read a recorded provider failure. The default scope is the canonical
current workspace. `--workspace` overrides it; `--all-workspaces` removes it; they are mutually
exclusive. Provider defaults to all providers. The limit defaults to 20 and must be from 1 through
200. This query never repairs or writes.

Search allows up to 5,000 event reads and 64 MiB of event data. It checks a ten-second time budget
between operations; that is not a hard command deadline. Human output prints tab-separated session,
provider, timestamp, request/event id, and excerpt, followed by counts and scan diagnostics. JSON
hits also include `result_command`. See the [JSON field reference](#json-field-reference). Excerpts
are limited to 200 displayed characters.

No hits exits 0, as in the example. If `incomplete` is true, read `incomplete_reasons` before
concluding that the text is absent; some evidence was skipped or unreadable. Narrow the workspace or
provider filter if the scan reaches its budget. If the default workspace cannot be resolved, pass
`--workspace PATH` or `--all-workspaces`. Invalid options and command-level read errors exit
nonzero; check the argument or path named in the error.

## doctor

Run `doctor` to investigate a missing executable, failed launch, or unavailable delivery path.

```text
tabcli doctor SESSION [--probe] [--json]
tabcli doctor --provider PROVIDER [--probe] [--json]
```

```sh
tabcli doctor --provider codex --json
```

```json
{
  "ok": true,
  "session": null,
  "provider": "codex",
  "probe": false,
  "checks": [
    {
      "id": "provider_version",
      "availability": "unknown",
      "reason_code": "probe_not_requested"
    }
  ]
}
```

Observe a session or provider, exactly one of the two. `--probe` defaults to off; it enables bounded
local capability/version probes, with a shared five-second probe budget. The Codex hook-trust
exchange has its own additional ten-second maximum. It does not launch a model turn,
deliver a prompt, start a daemon, repair records, or change settings. Historical launch
configuration and current observations are reported separately.

The report starts human output with `scope`, describing the inputs used by these checks:

- `session` is the requested session id, or null in provider-only mode; `provider` is the
  recognized provider, or null when absent or unrecognized.
- `executable` is the validated absolute executable path, or null if unusable.
  `executable_source` is `launch_record` for a usable manifest path, `resolved_path` for
  provider-only PATH resolution, or null. Session paths are never replaced through PATH.
- `current_version` is the result of a successful version probe; otherwise it is null.
  `configured.provider_version_at_launch` remains separate historical evidence.
- `model` and `effort` are the manifest's launch overrides, not effective runtime settings;
  both are null in provider-only mode.
- `workspace` is the manifest workspace or the current directory, or null if the scope-only
  current-directory lookup fails. The version probe runs in a scratch directory, so this
  field does not name every probe's working directory.
- `probe` records whether `--probe` was requested. `probe_budget_ms` is the shared 5000 ms
  budget captured at report start, not a fresh allowance for each probe.
- `consent_state` reuses the `state` from the existing `workspace_consent` observation, or
  is null when that state was not observed, including provider-only mode.

Scope is not proof of an effective runtime configuration or of delivery.

The session `owner` check is `unavailable` / `owner_exited` or `owner_identity_mismatch` only for a
bound owner attested gone or changed; a bound owner whose attestation could not decide is
`unknown` / `owner_unverified`, even when its process id looked absent.

Session reports append a `hold` check after the other record checks: `available` / `not_held`,
`unavailable` / `session_held`, or `unknown` / `hold_unreadable`. A held session's next action
names `hold SESSION --release` only when it is known not to be closed; a closed session retains
its hold as a record and cannot release it.

Session reports add an `origin` check, and `observations` gains `origin` and
`origin_observation` (see [origin observation](#origin-observation)). The check is `available` /
`origin_external` for an initial request from outside Bridge, `available` / `origin_live` when the
origin's attested owner is live, `unavailable` / `origin_ended` when the origin ended and this
session is not closed, `available` / `origin_ended_session_closed` when both have ended, and
`unknown` / `origin_unverified` otherwise, including when the session's own records could not be
read consistently. Only `origin_ended` suggests a command: an explicit `close-session` of this
session, if you no longer need it. The origin observation grants no close authority.
`doctor --provider` has no origin check.

`codex_lifecycle` reads only this session's records. Its `evidence.source` is `session_records`;
`launch_mode`, `input_hook_observed`, `stop_candidate_observed`, `observer` and `terminal_outcome`
each have `observation: observed|not_observed|unavailable`. An observed field has `record`;
an unavailable field has `reason`. Missing legacy records say `not_observed`. The row stays
`unknown`: it does not certify effective configuration. Human output prints the same evidence
object that `--json` places in `checks[].evidence`.

- `launch_mode.record` contains `lifecycle_hooks` (`injected|not_injected`), `reason`,
  `codex_version`, `vector_codex_version` and schema. A version mismatch adds `warning`.
- `claim_token` selects the current claim, otherwise the last request's claim.
  `input_hook_observed.record` contains the binding identity and `bound_unix_ms`;
  `stop_candidate_observed.record` includes `observed_unix_ms`, without candidate answer text.
- `observer.record` reports the recorded `armed`, `suspended` or `settled` state, reason,
  offset and observation time when present. `settled` is diagnostic, not publication proof.
- `terminal_outcome.record` comes from the last request's committed event, following the
  ordinary result publication rules. It includes request/event ids, provenance, request state,
  time and kind (`task_complete`, `turn_aborted`, `agent-turn-complete`, or `not_observed`).

Only with `--probe`, `codex_hook_trust` starts the installed Codex with the exact hooks override
this Bridge executable would inject, then sends stdio JSON-RPC `initialize`, `initialized`, and
`hooks/list` with this workspace. It ends the process tree after the response or ten seconds.
It creates no Bridge scratch files or configuration writes. This is `source: current_probe`,
not evidence about a session's live configuration or hook execution. Each of Bridge's two keys
reports `trustStatus`, `currentHash`, `expected_hash`, `hash_matches`, and `diagnostic`:
`normalization drift` when the hashes differ. Both trusted and matching means `available`;
untrusted/modified/mismatched means `unavailable`. Missing executable/keys, malformed replies,
errors and timeouts report `codex_hook_trust_unavailable` with a reason. Batch-only launchers
that require a disk forwarder are unavailable for this file-free probe.

The notify budget applies whenever a current request may still depend on notify: no binding
has been observed, or launch did not record injected lifecycle hooks. With both injection and
a binding it reports `available` / `codex_notify_payload_not_required`, even if the observer
is suspended; this never restores notify authority.

For Codex, `checks` includes `codex_notify_payload_budget`, an estimate of the pending turn's
`notify` command line against the native Windows limit of 32,767 UTF-16 units (see
[providers](providers.md#codex)). It reads the sizes of the framed prompt that the pending-turn
record keeps for the initial turn and for each `tell` follow-up, computes nothing beyond them, and
sends nothing. It is an estimate, not a measurement: the JSON-escaped framed prompt as sized before
delivery (the Windows console paste can drop a non-BMP character, so the delivered text can be
smaller), plus an allowance of 300 units for the payload's other fields and Codex's argument
quoting, plus the length of the doctor's own executable path, which stands in for the notify
executable fixed at launch because the records do not retain it. The real command line can be
larger or smaller: Codex's argument quoting adds a backslash for every quotation mark in the
payload, so text with many quotation marks is undercounted by that many units. `evidence` carries `prompt_chars`, `escaped_units`,
`envelope_units`, `limit_units`, `answer_budget_units` (the remaining units an answer can take),
`threshold_units` (4,096), `executable_source` (`doctor_executable`), `pending_request` when the
receipt is readable, and `source`: `pending_turn_record` for the retained sizes, or
`initial_prompt_record` when a pending turn installed before this version carries none and the
preserved initial prompt still exists. Off Windows it is `available` /
`codex_notify_payload_no_limit` with the same evidence. On Windows it is `unavailable` /
`codex_notify_payload_over_limit` when the estimate alone reaches the limit (the completion has
probably failed to reach Bridge), `unavailable` / `codex_notify_payload_near_limit` when fewer
than 4,096 units remain for the answer, and `available` / `codex_notify_payload_within_limit`
otherwise. It is `unknown` / `session_required` without a session, `unknown` /
`codex_turn_not_pending` when no Codex turn is pending, and `unknown` /
`codex_pending_prompt_unavailable` when the pending-turn record cannot be read or when a turn
installed before this version has lost its initial prompt record. If the request stays pending
while the session is working after a long turn, read the answer from Codex's rollout record and
close the session; never resend. Otherwise keep prompts and requested answers short on Windows.

For Pi, `checks` includes `pi_provider_credentials`. With a session's explicit
`provider/model` and `--probe`, Pi's local, non-refreshing auth check reports
`evidence.status` as `ready`, `not_ready`, or `unknown`, alongside `provider`, `authType`,
`exit_code`, `model`, and `executable`. The last two identify the intended target where
known, even on an early return; their presence does not mean a probe ran. The detail explains
the reason; `ready` means configured, not accepted.
Without a session model (including `doctor --provider pi`) or without `--probe`, the
status is `unknown`. Pi confirms model resolution before the provider check; no default
model or provider is guessed, no credential is printed, and no model call is made.

Human output prints checks, availability, reason codes, detail, evidence, and suggested next
actions. Availability is `available`, `unavailable`, or `unknown`. See the [JSON field
reference](#json-field-reference).

A completed report exits 0 even when checks are unavailable or unknown. `ok` means the report was
produced, not that delivery will work. Parser and report-generation errors fail. A missing
executable, unreadable ownership evidence, or unavailable messaging appears with a `next_action`.
Follow that action, or the suggested `next_command` argument array. In the example, add `--probe` to
read the installed version. A passing version check alone does not prove delivery will work.

## self-test

Run `self-test` to check a real initial request, follow-up, and close with your installed setup.

```text
tabcli self-test PROVIDER [--workspace PATH] [--terminal TERMINAL]
    [--model MODEL] [--effort EFFORT] [--yolo] [--timeout-secs N] [--isolated] [--json]
```

```sh
tabcli self-test codex --workspace /path/to/project --json
```

```json
{
  "provider": "codex",
  "isolated": false,
  "session": "session-T3b8Wq",
  "outcome": "passed"
}
```

This opens a visible surface and makes real model calls through the public commands. It checks the
initial result, a follow-up result, and cleanup. Workspace, terminal, model, effort, bypass, and
JSON options follow `ask`; prompts and titles are generated internally. `--timeout-secs` defaults to
120 per command, not for the entire test. Cleanup calls `close-session` with
`--timeout-secs 10` and a 15-second outer budget so the child can report its timeout before
self-test ends it. These budgets do not guarantee that the close finishes.

Both turns ask: `No tool, command, or file is needed. Reply with exactly this marker and
nothing else: <marker>`. The result must still equal the generated marker exactly.
On an Agy result timeout, self-test reads `doctor SESSION --json` before cleanup. If it
reports a tool confirmation for that request, the step's `reason` includes the tool and
approval observation. Without that evidence, the timeout reason is unchanged. The step
remains `timed_out`; Bridge does not answer the approval or resend the request. `result`
and `inspect` retain their existing output; use `doctor` for this Agy log diagnosis. On a
Pi result timeout, self-test calls `doctor SESSION --probe --json` within five seconds
and appends the same session's credential readiness to the step reason. This is a
configuration observation, not proof of why the turn timed out; the request stays pending
and claimed until ordinary cleanup closes the session.

`--isolated` defaults to off. Normally the test uses the ordinary state root, including its settings
and consent records. Isolation creates a private directory; ordinary settings and consent do not
apply. The directory remains afterward. In either mode, the test closes only sessions it created,
retains closed records, and never resends uncertain input. The test uses `ask`’s workspace-consent
rules, including reuse of verified existing trust. It does not grant consent for an untrusted
workspace. `--isolated` separates Bridge records, not provider trust stores.

Before it creates a private directory or runs any public command, the test resolves the workspace
and applies [`ask`](#ask)'s launch admission. The ordinary mode uses the ordinary state root's
`codex-memory-workspace` setting. The isolated mode copies no setting and always uses the default,
`refuse`, so it refuses the Codex memory workspace and adds `Isolated self-test uses default
settings; use ordinary self-test with the allow setting to permit this workspace.` to the reason. A
refused test runs no `ask`, discovery, or cleanup. Its report has `outcome: "failed"`, null
`session` and `session_state`, one step `{"name": "admission", "outcome": "failed", ...}` with the
reason, and empty `cleanup_sessions`. `state_root` is the ordinary root, or null in isolated mode
because no private directory was created; human output then prints `state root: not created`.

Human output reports the mode, CLI, terminal, state root, session, each step, cleanup, and overall
outcome. In JSON, read each failed step’s `reason` and `request_address`. See the [JSON field
reference](#json-field-reference).

Exit 0 requires a fully verified round trip and cleanup. Outcomes are `passed`, `failed`,
`timed_out`, `unsupported`, and `not_verified`. Authentication failures, unsupported delivery, trust
dialogs, wrong results, or unverified cleanup prevent success. Read the failed step's `reason`.
Split `request_address` at `/` and run `tabcli result SESSION --request REQUEST`. Resolve
authentication in the provider CLI or answer a trust dialog yourself. If cleanup failed, inspect the
reported session and use `close-session SESSION --explicit`; do not rerun the model prompt to test
cleanup. An unconfirmed close is `not_verified`, including a close that times out. Its reason
names the session and includes the recorded launch error when a failed launch retained a surface.
A closed session with an unverified residual surface makes cleanup `not_verified` even when
close itself succeeded. If self-test's explicit close confirms that surface is gone, cleanup passes
even though the launch failed.

## consent

Use `consent` to inspect or stop Bridge's reuse of workspace trust for a directory.

```text
tabcli consent inspect|revoke|reset PATH [--json]
```

```sh
tabcli consent revoke /path/to/project
```

```json
{
  "schema": 1,
  "identity": {
    "path": "/path/to/project",
    "volume": 16777234,
    "file": 2849017,
    "created": 1791284000000000000,
    "owner": "501"
  },
  "source": null,
  "revoked": true
}
```

Inspect or change Bridge's workspace consent record for an existing directory. `inspect` reads its
identity and saved record; it does not grant trust. `revoke` blocks Bridge's reuse of that consent.
`reset` clears the saved source and revocation, allowing future assessment. Neither operation edits
the provider's trust store or changes its tool permission mode.

There is no default action or path. `--json` is optional and off by default but has no output format
effect: all three actions print JSON. See the [JSON field reference](#json-field-reference) for the
record fields.

Successful reads/writes exit 0. An absent Bridge record is not an inspection error. Missing or
non-directory workspaces, unsafe store access, or unreadable records fail. Check the directory and
ownership named in the error; do not make a trust store writable by other accounts to get past the
check. See [security and data](security-and-data.md) for exact-workspace evidence and store
protection.

## settings

Use `settings` to choose where future sessions open their terminal surfaces, and whether a launch in
the Codex memory workspace is admitted.

```text
tabcli settings [--json]
tabcli settings windows-tab-window dedicated|current [--json]
tabcli settings macos-open-mode tab-first|new-window [--json]
tabcli settings codex-memory-workspace refuse|allow [--json]
tabcli settings codex-lifecycle-hooks on|off [--json] # default: on
```

`codex-lifecycle-hooks` controls hook injection for new Codex launches only. It writes Bridge's
setting, never Codex configuration. Existing sessions retain their recorded launch mode and
Windows composer gate. Injection requires stable Codex 0.163.0+ and a supported executable
path; it is not proof of hook execution. See [Codex](providers.md#codex) for trust hashes,
picker recovery, transcript completion and the unbound notify fallback.

```sh
tabcli settings
```

```json
{
  "settings_file": "/path/to/home/.agent-bridge/native-sessions/settings.json",
  "windows_tab_window": "dedicated",
  "macos_open_mode": "tab-first",
  "codex_memory_workspace": "refuse"
}
```

With no setting, read effective settings. Otherwise persist one setting in `settings.json` under the
selected state root. Defaults are `windows-tab-window dedicated` and `macos-open-mode tab-first`.
The Windows setting chooses the dedicated Agent Bridge window or the most recently used Windows
Terminal window; the macOS setting requests a tab first or always a new window. Each affects
creation on its platform. Later changes do not change a surface's recorded close scope. See
[terminals](terminals.md).

`codex-memory-workspace` defaults to `refuse`: `ask`, `reopen`, and ordinary `self-test` refuse a
workspace that is the Codex memory workspace or lies inside it, for every provider (see
[`ask`](#ask)). `allow` admits such launches, including Codex's own background delegations; `refuse`
restores the default. It is a launch admission preference, read at each launch, and grants or
removes no close authority over existing sessions. Isolated `self-test` does not read it.

`--json` is optional and has no format effect: output is always JSON, with `settings_file`,
`windows_tab_window`, `macos_open_mode`, and `codex_memory_workspace`.

Successful reads/writes exit 0. Unknown setting names or values, duplicate `--json`, an invalid
settings schema, and read/write failures fail. An invalid admission value fails with
`codex-memory-workspace is refuse or allow, not "VALUE"`. Use one setting/value pair from the synopsis. For
record errors, inspect the `settings_file` path or the path in the error before changing it. A
missing record uses defaults.

## prune-sessions

Use `prune-sessions` to delete closed session records you no longer need.

```text
tabcli prune-sessions --closed-before-days N --explicit [--json]
```

```sh
tabcli prune-sessions --closed-before-days 30 --explicit
```

```text
pruned session-B2n6Yh
pruned 1 closed session(s)
```

Permanently remove eligible closed session directories. Both options are required: `N` must be a
positive supported whole day count, and `--explicit` acknowledges deletion. There is no implicit
retention policy or dry-run flag.

Both the close tombstone and status must say `closed` and be at or before the retention cutoff.
Active capabilities or a blocking owner prevent pruning. Invalid, unsafe, or ineligible entries are
skipped; pruning never closes a running session to make it eligible.

Human output says `pruned SESSION` per removal and gives a count, or `no eligible closed Agent
Bridge sessions`. JSON has `ok`, `closed_before_days`, and `pruned`.

No eligible sessions is success. Invalid retention, missing acknowledgement, and I/O or owner
inspection errors can fail. Correct the retention value or include `--explicit` only when you intend
to delete the records. For an I/O or owner error, inspect the named session before retrying. Removal
is per directory; a later failure does not undo earlier removals. Preserve any results you need
before pruning.

## close-session

Run `close-session` when you have finished with a session or want to stop waiting for it.

```text
tabcli close-session SESSION --explicit [--with-started-from] [--timeout-secs N] [--json]
```

See the command and output in the [opening workflow](#command-reference).

`--explicit` is required because this ends the session's surface. You can close active, exited, or
failed sessions. Repeating a completed close succeeds without closing another surface; with
`--with-started-from` it can still discover and close another session started from this one
(see below). Bridge records the close in a tombstone and keeps the result history.

`--timeout-secs N` sets the maximum time the close may take, in whole seconds from 1 through
3600; the default is 60 seconds. The budget covers observation, ownership checks, lock waits,
process teardown, and terminal automation. It does not guarantee that the close finishes.
A timeout means the close was not confirmed and may have partly happened. A rerun observes
the session and establishes authority again; it may still refuse the close.

Every open session loads its terminal, including finished sessions that were never closed. When
you delegate many tasks, cap the number of open sessions, count finished but unclosed ones, and
close a session before you open its replacement. If the terminal answers slowly, give the close a
longer `--timeout-secs`.

On macOS, a terminal application can end while the session's processes survive, as observed
after iTerm2 was killed. If the ownership proof fails, Bridge checks whether the recorded
surface is absent. Terminal.app requires its incarnation-aware absence proof. Proven absence
and owner attestation let Bridge record a teardown intent, attest the owner again, and send
SIGTERM to the process group the owner leads. It sends no SIGKILL and no signal to the shell.
Bridge waits up to three seconds for ESRCH to prove the group is gone, then records `closed`
with outcome `Missing`, consumes the handle, and retains the reason in `inspect`.

If disappearance cannot be confirmed, close fails and keeps the intent and handle. Check the
error and the session's processes before repeating `close-session SESSION --explicit`. A
retry with a live attested owner obtains authority again. With a dead owner, it sends no signal
and completes only when the recorded group is proven gone. EPERM, including for a group
containing only zombies, is not proof of disappearance. Observation and signal are not one
atomic operation. The group's disappearance does not prove that processes which left it ended.

While a session is `launching` with no surface handle and a pending launch deadline still in the
future, close refuses without changing the session or its claim:

```text
the launcher is still creating the surface for this session (launch deadline <unix ms>); no handle exists yet and nothing was closed; close again after the deadline or once the session has failed
```

After the deadline, or once launch is no longer pending, a handle-less close proceeds. If the
launcher subsequently reports a retained Ghostty or Warp surface, the session stays closed with no new
handle. Its error names the exact surface and says it may remain and is not closed by Bridge.
The warning is appended to any existing error and survives repeated close. Check the surface
in that terminal; another Bridge close does not close it.

If the failed-launch handoff happens before close, while the launch still owns its claim and the
session is still `launching`, the launcher saves the proven terminal, tab, and window ids in
`terminal.json` and records `failed`. Explicit close uses that handle through
Ghostty's close script, even if no provider started. It records `closed` only after confirmed
closure; another close failure keeps the handle.

For Warp, a proven tab whose control-binding write and exact cleanup both fail is also saved
in `terminal.json`, with `failed` status and a residual-surface warning naming its ids. A confirmed
cleanup saves no handle. The retained ids do not replace Warp's missing app incarnation/control
binding, and ownerless startup authority still permits absence verification only. Recovery close
for this failure is not implemented (#87); an explicit close refuses and preserves the handle
when absence cannot be proved. Check the named tab in Warp. This failure has not been observed
live, and Warp remains not live-verified (#63).

After a successful close, and only then, Bridge looks once for sessions whose initial request
names the closed session as its origin and that are not closed in their records. The scan reads
records only: per session the manifest, the close tombstone and status, and for a session that is
not closed its launch record and the one receipt of its initial request. It reads no request index,
event, owner process or surface, and it closes and repairs nothing. It uses what is left of the
close deadline, at most two seconds, and stops at 4,096 sessions, 64 KiB per record, 16 MiB in
all, or 256 sessions found. The deadline is checked between reads; a slow filesystem call can
outlast it. A scan that stops early or cannot classify a session is reported as incomplete; it
never fails the close. The answer describes the records at the time of the scan: sessions can be
created, closed or changed during and after it, and a session launched from this one but not yet
recorded is not found.

Without `--with-started-from`, human output is `closed SESSION`. When sessions were found it adds
`N session(s) started from SESSION remain unclosed in the observed records: ID ...` and
`Close a session you no longer need with 'tabcli close-session ID --explicit'.`; an incomplete
scan adds `Started-from scan incomplete; the list may omit sessions.`. JSON has `ok`, `session`,
`closed`, and `started_from` (see the [JSON field reference](#close-session-1)).

### Closing the sessions started from it

`--with-started-from` closes, after SESSION, each session that the scan listed, one level
deep: a session started from a listed session is reported, never closed. It requires
`--explicit` like the close itself; without the flag the command and its output are unchanged.
The scan is the one described above, with the same bounds, and runs only after SESSION closed.

Each listed session goes through the same close as an explicit `close-session` of it: the
dead-owner repair, the owner attestation and the terminal-close authority checks. The
recorded origin grants nothing; a session that its own explicit close would refuse is refused
here with the same error. Immediately before each close, Bridge re-reads the session's records
and compares them with what the scan saw. A session that is not what the scan listed is
skipped and not closed. The skip reasons are `closed_since_scan` (its records now say closed),
`source_changed` (its initial request now names another source), `session_gone` (its directory
disappeared or was replaced), `session_changed: <what>` (its status, generation, launch Claim,
initial receipt, source or current Claim changed, or its current Claim could not be read),
`byte_limit`, or one of the scan's own classification reasons such as `close_in_progress`.
A target is closed at most once and never retried.

`--timeout-secs` is the one budget for the close of SESSION, the scan and every target close.
A target reached after the deadline, or whose verification or close outruns it, is `failed`
with `time_budget_exhausted`; the remaining targets get the same verdict, so the report covers
every listed session. The scan's own two-second cap does not reserve time for the closes: a
scan that stopped early still has its listed sessions closed while the deadline permits, and
an exhausted scan with no listed session exits 0 with `incomplete: true`, which does not
establish that every session started from SESSION was closed.

After each target closes, Bridge runs the same bounded scan for that target and attaches the
result to its entry, so the sessions started from a closed target are visible without being
closed. Sessions started from SESSION after the scan, or recorded only later, are not found.
Repeating the flagged command can discover and close another session started from SESSION.

Human output is `closed SESSION`, then `started-from candidates: N (scan complete|incomplete)`,
then one line per listed session in id order: `closed ID (started from SESSION); its own
started-from: K open[, incomplete]`, `skipped ID: REASON`, or `failed ID: ERROR`; then
`started-from close: C closed, S skipped, F failed`. The "remain unclosed" lines are not printed.
JSON is one object with `ok`, `session`, `closed`, and `started_from`, which adds `closed`,
`skipped`, and `failed` (see the [JSON field reference](#close-session-1)).

Exit 0 means SESSION closed and no listed session failed; skipped sessions do not fail the
command. When any target failed, the full report is printed first, JSON or human, and the
command then exits non-zero with
`close-session closed SESSION but N of M started-from session(s) failed to close; see the report`
on stderr. In JSON, `closed` is `true` and `ok` is `false` then. A failure of the close of
SESSION itself is reported as without the flag, and no target is closed. For a failed target,
check its error, then close it with its own `close-session ID --explicit` once resolved.

Exit 0 means Bridge completed the close. If the id is missing, check it with `sessions`; if the
command asks for acknowledgement, add `--explicit` only when you intend to close that session. For
refused close authority, unreadable records, or terminal errors, run `doctor SESSION` and check the
surface yourself. Bridge keeps the handle so you can retry the explicit close after resolving the
error. If close was interrupted, the next close uses its saved records to finish; a dead PID alone
does not establish that the surface closed. See
[ownership and close](terminals.md#ownership-and-close).

## Shared prompt and output options

Use these options only with commands whose synopsis lists them:

| Option | Default and meaning |
| --- | --- |
| `--prompt TEXT` | Required unless `--prompt-file` is used; the two are mutually exclusive. |
| `--prompt-file PATH` | Read UTF-8 text and normalize CRLF to LF; no stdin special case. |
| `--timeout-secs N` | Positive whole seconds; defaults below. |
| `--detach` | Off. Return after launch/delivery acceptance, without waiting for a result. |
| `--json` | Off. Select structured output, except commands that always print JSON. |
| `--context-result SESSION/REQUEST` | None; up to eight attachments on `ask` or `tell`. |
| `--context-mode full\|compact` | `full`; `compact` attaches an excerpt of 4,096 + 1,024 Unicode scalar characters with the retrieval command and records `mode`, `attached_chars`, `omitted_chars`. |

`--timeout-secs` defaults to 900 for `ask`, `tell`, `reopen`, and `result --wait`, and to 120 per
`self-test` command. For `close-session`, it defaults to 60 and accepts 1 through 3600.
The value must fit the supported clock range.

Prompts must contain non-whitespace text. Control characters other than newline and tab are
rejected. Bridge trims the prompt's outer whitespace and adds delegation framing and provider
correlation material. `--detach` still waits for the required launch and delivery steps; it is not a
request to skip those checks. When you see `accepted` and a null `result`, use `result SESSION
--request REQUEST --wait` to wait for the result.

For `ask`, `tell`, and `reopen`, the timeout is one command deadline shared by preparation, launch
or delivery, and result waiting. A timeout while waiting for a result does not cancel delivered
work. A startup timeout can prevent a provider that has not started from launching. Startup has a
separate ceiling of 30 seconds or the remaining command budget, whichever is shorter; increasing
`--timeout-secs` does not extend that ceiling. Inspect the recorded request before retrying. Bridge
allows one request at a time through a claim, the exclusive right to deliver that turn. If delivery
is uncertain, it keeps the claim: another `tell` is blocked until completion or explicit close. Wait
for that request or close the session; do not resend it.

Address an attachment as `SESSION/REQUEST`, using the ids printed by `ask` or `tell`. For `result`,
pass them separately as `result SESSION --request REQUEST`. Context attachments must resolve to
published successful results before any session is created or any target is changed. Failed,
pending, unreadable, and unpublished results are rejected. Duplicate addresses and results with
unsafe control characters or missing creation times are rejected. A result containing its own
attachment delimiter is also rejected. The combined attached result bodies are limited to 256 KiB;
attach fewer or smaller results if you exceed that limit. Bridge does not truncate attachments to
make them fit. Results are appended as delimited reference material and their provenance is saved as
`context_sources`. For older records without a request receipt, `SESSION/EVENT` is accepted instead.
This is reference material, not a resume of the source conversation.

`--context-mode full` is the default and keeps the attachment output unchanged. `compact`
keeps messages of at most 5,120 Unicode scalar characters whole; longer messages retain the
first 4,096 and last 1,024, separated by a visible elision with the omitted count and the exact
`tabcli result SESSION --request REQUEST` retrieval command (`--event EVENT` for legacy events).
Each compact block reports attached/total/omitted characters and completed state, plus the
number of recorded question emissions when available. Compact never changes which Results
are attachable: the whole original message still undergoes the same validation. Its rendered
excerpt, including the elision line, counts toward the 256 KiB attachment budget, allowing
long results that exceed the full-text byte budget. No mode silently shortens text to fit.
Both modes record `mode`, `attached_chars`, and `omitted_chars` on each context source;
`full` records the whole character count and zero omitted. Old receipts omit these fields.
An explicit mode requires at least one `--context-result`; repeated, empty and unknown modes
are rejected before reading sessions. `reopen` accepts neither attachment option.

Successful commands normally exit 0; parsing, execution, and I/O failures exit nonzero and print an
error on stderr. Check the exit behaviour in each command section: a successful observation can
report a failed request or an unavailable capability. After arguments have been parsed successfully,
with `--json`, every failure of `ask`, `tell` and `reopen` prints one JSON error object: with the request
address once a request exists (and the recorded request state when it can be read), and with the fields
the command already knows before one exists (`session` for `tell`, `source_session` and `gate` for `reopen`). Argument-parsing
failures of these three commands still report on stderr without a JSON error object.
Human result text has terminal control characters escaped.
The JSON examples in this reference are trimmed excerpts with invented values, not complete response schemas.

Unknown options, repeated single-value options, missing values, and invalid ids fail before the
command runs. Copy ids from command output rather than constructing them. Session ids begin with
`session-`, contain only ASCII letters, digits, hyphens or underscores, and are at most 96 bytes.
Request ids begin with `request-`, have a nonempty suffix, contain only ASCII letters, digits and
hyphens, and are at most 160 bytes.

## States and observation boundaries

Session states are `launching`, `running`, `awaiting-initial-input`, `ready`, `claimed`, `working`,
`resume-pending`, `exited`, `failed`, and `closed`. Older unknown strings are preserved; queries can
report `unknown` for unreadable state. `tell` accepts only `ready` and not held. `reopen` requires `closed` plus
provider-specific evidence. Queries accept any recorded state; they can still fail on unreadable
required records. `prune-sessions` requires `closed` records with no active capability or blocking
owner.

Hold is orthogonal to session state and refuses only follow-ups whose delivery Bridge has not yet
allowed to start.

Request states reported by `result` are `completed`, `failed`, `cancelled`, `pending`, `unresolved`,
`recovery_required`, and `unavailable`; a waiting read can report `busy` when it times out without a
consistent observation. `cancelled` occurs only for a correlated provider interruption (see
[cancel](#cancel)). `accepted` is the detached command acknowledgement, not a published result.
A follow-up or initial prompt that Bridge refused before anything was sent (a preparation, budget,
hold or transport refusal) is `failed`: Bridge records a failure event for the receipt whose
`error` is the refusal text and whose `provenance` is `delivery_not_sent`, with no provider
session or turn id. `unresolved` is a receipt without any event: a refusal recorded by an earlier
Bridge release, a reopened-conversation refusal, or a claim-stage race. Bridge never infers a
refusal from `status.error` and never repairs such receipts.
Error fallback output can use `unknown`. Request and session state are different: a session can
remain usable after a request completes, and a closed session can retain results.

`status`, `inspect`, timeline, `result`, `wait`, `search`, and `doctor` only observe. They never repair, recover,
resend, close, or write records. The public `sessions` command performs repair as documented above;
its origin fields and the started-from report of `close-session` only observe.
No query treats a missing result as an instruction to deliver the prompt again.

## Environment and internal interfaces

Set `AGENT_BRIDGE_NATIVE_STATE_DIR` to choose the root for sessions, settings, and consent. Without
it, Bridge uses `.agent-bridge/native-sessions` beneath `HOME`, or `USERPROFILE` if `HOME` is
absent. Provider lookup uses absolute entries in `PATH`; platform helpers also use `PATH` where
their adapters require it.

Provider trust lookup follows these environment settings:

- `CODEX_HOME`: read `config.toml` here; the default is `.codex` under home.
- `CLAUDE_CONFIG_DIR`: read `.claude.json` here; the default directory is home.
- `PI_CODING_AGENT_DIR`: read `trust.json` here; the default is `.pi/agent` under home.

On macOS, `TERM_PROGRAM`, `TERM`, `ITERM_SESSION_ID`, and `TERM_SESSION_ID` select the terminal as
described under [terminal selection](terminals.md#selection-and-settings).

Use the state-root override to choose a separate registry; it is not a switch that isolates the
provider's credentials or prevents network requests. Bridge runs no hosted service of its own and
stores its records locally; provider CLIs still contact their services.

The `native-*` commands and terminal host commands exist for managed surfaces, provider hooks, and
internal control. Their arguments, session-marker environment variables, and test-aid variables are
not a public interface. Do not invoke them as substitutes for public commands.

## JSON field reference

Use this reference when consuming command output in a script. The examples above omit fields; these
lists describe the success outputs, including nested objects where listed. Error output can be
shorter, as described under [shared output rules](#shared-prompt-and-output-options).

### ask and tell

Fields: `ok`, `schema_version`, `session`, `request_id`, `context_sources`, `request_state`,
`provider`, `terminal`, `terminal_session_id`, `terminal_tab_id`, `terminal_window_id`,
compatibility field `iterm_session_id`, `result`, `provider_session_id`, `turn_id`,
`bridge_observed_elapsed_ms`, and `bridge_observed_elapsed_reason`. A detached success has
`request_state: "accepted"` and a null `result`; a waited success has `request_state: "completed"`.

A pre-claim Hold refusal in `tell --json` has `schema_version`, `ok:false`, `session`, and `error`, without
`request_id`; a claim-stage race can leave an unresolved receipt visible through `result --list`.

### hold

Fields: `schema_version`, `ok`, `session`, `held`, `changed`; malformed intent adds
`previous: "malformed"`. Errors have `schema_version`, `ok:false`, `error`, and `session` when
resolved by the command. `changed` means the hold intent changed.

### cancel

Success fields: `schema_version`, `ok`, `session`, `request_id`, `state` (`requested`),
`created_unix_ms`. Refusals return `schema_version`, `ok: false`, `session`, and `error`;
parse errors may omit `session`.

### self-test

Fields: `schema_version`, `bridge_version`, `provider`, `provider_version`, `terminal`,
`state_root`, `isolated`, `marker`, `session`, `session_state`, `outcome`, `elapsed_ms`, `steps`,
and `cleanup_sessions`. Steps contain `name`, `outcome`, `elapsed_ms`, `request_address`,
`event_address`, and `reason`; cleanup entries contain `session`, `session_state`, `outcome`, and
`reason`. An Agy result timeout can append the same request's tool-confirmation observation
from `doctor` to the existing step `reason`; a Pi result timeout can append the session's
provider credential readiness. No field is added. `state_root` is null only when launch admission
refused an isolated test before its private directory existed; that report has one step,
`admission`.

### settings

Fields: `settings_file`, `windows_tab_window` (`dedicated` or `current`), `macos_open_mode`
(`tab-first` or `new-window`), and `codex_memory_workspace` (`refuse` or `allow`). There is no
`schema_version`.

### sessions

Each array entry contains `id`, `provider`, `workspace`, `title`, `yolo`, `state`, `terminal`,
`terminal_session_id`, `terminal_tab_id`, `terminal_window_id`, `iterm_session_id`,
`created_unix_ms`, `updated_unix_ms`, `error`, `model`, `effort`, `resumed_from`, `results`,
`origin`, and `origin_observation`. The event count is not a count of successful published
results. The origin fields are described under [origin observation](#origin-observation).

### status

The envelope contains `schema_version`, `ok`, `filters`, `sessions`, `incomplete`,
`incomplete_reasons`, and `scanned`. Filters contain `workspace` (canonical path or null),
`all_workspaces`, `provider` (or null), and `include_closed`. Scan counts contain `sessions`
(observation attempts after scope filtering) and `listed`. Each incomplete reason has
`session` (null for a root-level problem) and `reason`.

Each session contains `id`, `provider`, `workspace`, `title`, `model`, `effort`, `yolo`,
`created_unix_ms`, `state`, `generation`, `updated_unix_ms`, `error`, `turn_claimed`,
`recovery_required`, `unreadable_requests`, `request_index_error`, `active_request`,
`latest_result`, `owner`, `attention`, `derived_from`, `result_command`, `origin`, and
`origin_observation`.
`held` is true for a valid hold, false for an absent record (including legacy sessions), or null
with `hold_error` when the record cannot be read. A true value adds attention `held`; null adds
`records_partially_unreadable`. `residual_surface` follows inspect's optional-field rule below.

`active_request` is null or contains `request_id`, `request_state`,
`bridge_observed_elapsed_ms`, and `bridge_observed_elapsed_reason`.
`latest_result` is null or contains `event_id`, `request_id`, `request_state`, and
`created_unix_ms`; it never includes the body. Legacy results can have a null request id.
`owner` contains nullable `process_alive`, `identity_matches`, and `error`.
`derived_from: "observation"` marks attention and the request objects as judgments.

Attention flags are `cancel_requested`, `held`, `delivery_unconfirmed`, `launch_timeout`, `launch_failed`,
`launch_uncertain`, `claim_without_receipt`, `recovery_required`, `owner_exited`,
`owner_identity_mismatch`, `owner_unverified`, `records_partially_unreadable`,
`request_index_incomplete`, `residual_surface_unverified`, `session_failed`, `session_exited`,
and `origin_ended`. No flag supplies repair or close authority.

### Origin observation

`status` sessions, `sessions` rows, `inspect` and `doctor` `observations` carry the same two
fields. `origin` is the id of the managed origin session, or null. `origin_observation` is:

```json
{
  "kind": "managed",
  "request_id": "request-example",
  "stored_state": "closed",
  "ended": true,
  "reason": "recorded_closed",
  "detail": null,
  "derived_from": "records"
}
```

`kind` is `managed`, `external`, or `unknown`. `request_id` is the initial request whose receipt
named the origin, or null. `stored_state` is the origin's recorded state, or null when it was not
read. `ended` is true, false, or null when it could not be decided. `detail` is a nullable
explanation. `derived_from` is always `records`.

The origin is taken only from the receipt of the claim that the session's launch record names.
No other receipt replaces it: a follow-up from another session, an older timestamp, or a
`resumed_from` source never becomes the origin. A reopened session's origin is the caller of
`reopen`. The recorded source is the caller's environment at the time of the request, not an
authenticated identity.

| `reason` | `kind` | `ended` | Meaning |
| --- | --- | --- | --- |
| `recorded_closed` | managed | true | The origin's close tombstone or status records `closed`. A status that has not caught up with the tombstone is named in `detail`. |
| `recorded_failed`, `recorded_exited` | managed | true | The origin's status records that state. |
| `owner_exited` | managed | true | The origin's bound owner process is attested gone. |
| `owner_reused` | managed | true | The owner's process id now belongs to another process, which grants no authority. |
| `owner_live` | managed | false | The owner is attested live; `detail` notes a process-id-only record. |
| `owner_unverified` | managed | null | No owner record, an unbound or foreign one, or an attestation that could not decide. |
| `origin_missing` | managed | null | No session directory with that id in this state root; a pruned, deleted or foreign session looks the same. |
| `origin_unreadable` | managed | null | A linked or damaged directory, manifest, status or tombstone. |
| `origin_unverified` | managed | null | A recorded state this version does not know. |
| `origin_busy` | managed | null | The origin's records changed while they were read. |
| `origin_budget_exhausted` | any | null | The read budget ran out. |
| `origin_external` | external | null | The initial request came from outside Bridge. |
| `initial_request_unavailable` | unknown | null | No launch record, an invalid one, or no receipt for its claim, as for sessions launched before 0.0.7. |
| `initial_receipt_unreadable` | unknown | null | The initial request's receipt could not be read. |
| `source_unavailable` | unknown | null | The initial receipt records no source. |
| `invalid_source` | unknown | null | The recorded source is not a session id or names the session itself. |

Only the origin itself is read, in the same state root, through bounded regular-file reads: at
most 100 ms, 64 KiB per record and 512 KiB per origin. No lock is taken and no record is written;
origins of origins are not followed.

### close-session

Success fields: `ok`, `session`, `closed`, and `started_from`. There is no `schema_version`.
`started_from` contains `open`, the sorted ids of sessions found (at most 256);
`incomplete`; `incomplete_reasons`, at most 64 objects `{session, reason}` where `session` is null
for a root-level reason; `additional_incomplete_reasons`, the count of reasons beyond 64; and
`scanned` with `sessions` (sessions examined, excluding the closed one) and `listed` (the length of
`open`). A reason starts with a code and may add context after a colon: one of the
[origin observation](#origin-observation) reasons for initial evidence,
`records_unreadable`, `close_in_progress`, `session_changed`, `time_budget_exhausted`,
`entry_limit`, `byte_limit`, `open_limit`, `root_unreadable`, or `entry_unreadable`. `incomplete:
false` does not mean that no session will be launched from the closed one later.

With `--with-started-from`, `started_from` keeps those fields, where `open` is the list of
candidates before any target close, and adds `closed`, `skipped`, and `failed`, each in
candidate order and together covering every id of `open`. `closed` holds objects
`{session, started_from}`, where `started_from` is the same report-only scan object for that
closed target (its `open` sessions are listed, not closed). `skipped` holds `{session, reason}`
with `closed_since_scan`, `source_changed`, `session_gone`, `session_changed: <what>`,
`byte_limit`, or a scan classification reason. `failed` holds `{session, error}`, where `error`
is the target's own close error or `time_budget_exhausted`. `closed` at the top level says that
the named session closed; `ok` is `true` only when `failed` is empty, and the command exits
non-zero after printing the object when it is not. The object is still printed without a
`schema_version`.

### inspect

Fields: `schema_version`, `ok`, `session`, `provider`, `workspace`, `title`, `stored_state`,
`generation`, `created_unix_ms`, `updated_unix_ms`, `error`, `exit_code`, `configured`,
`resumed_from`, `workspace_consent`, `owner_process_alive`, `owner_identity_verified`, `owner`,
`recovery_required`, `turn_claimed`, `unreadable_requests`, `request_index_error`,
`recorded_events`, `latest_result`, `requests`, `origin`, and `origin_observation`. `configured` contains `model`, `effort`, `yolo`,
and `provider_version_at_launch`. Request references include `request_id`, `created_unix_ms`,
`source`, `event_id`, `context_sources`, `active`, and the two elapsed-time fields described under
`result`.

Owner inspection separates a known identity mismatch from an inspection failure. For any
owner record, `owner.identity_matches: true` means every recorded identity field matched;
`false` means a verified mismatch of a recorded field, with `owner.error: null`. This also
applies to intermediate macOS records containing birth and TTY but no process groups; it
does not assert that unrecorded fields were verified. PID-only or otherwise unattested
records keep the match null; an unreadable identity also keeps it null and reports an error.
Launch repair fails an unconfirmed launch on a mismatch, and status and doctor report
`owner_identity_mismatch`; a pending public wait ends as unresolved only when the process id was
reused. A verified intermediate
record can therefore report an available owner in doctor; an inspection failure remains
unverified. These effects apply to macOS intermediate records as well as Windows records.
Completed Results still take precedence. These observations alone grant no general Surface
close authority.
Only an owner record bound to the session is attested. A record of another managed session is not
observed: `owner.error` is `owner record belongs to another session`, `owner.process_alive`,
`owner.identity_matches` and `owner_process_alive` are null, and `owner_identity_verified` is false.

`held` is true, false, or null with `hold_error`, using the same rules as `status`.
`cancel` is null when absent, otherwise `{request_id, requested_unix_ms, state}`; `state`
is `requested`, `applied`, `not_applied`, or `unreadable`. Unreadable evidence includes `error`
and may have null identity and time. Status adds `cancel_requested` only for an active Claim
whose cancel is still requested. Timeline retains a `cancel_request` entry and labels its
outcome interpretation with `derived_from: "result"`.
These fields describe current intent; they do not add hold history to timeline.

`residual_surface` is optionally `"unverified"` when a failed launch left a surface whose cleanup
has not been confirmed. It persists independently of `error`, including after a handle-less
close. Only the terminal adapter's confirmed close or absence clears it. Older records with
the residual-surface warning are recognized without being rewritten. Ordinary inspections omit
this field; it grants no additional close authority.

### result

Single-result fields: `schema_version`, `ok`, `session`, `provider`, `workspace`, `request_id`,
`event_id`, `context_sources`, `request_state`, `session_state`, `result`, `error`, `session_error`,
`provider_session_id`, `turn_id`, `created_unix_ms`, `recovery_required`, `unreadable_requests`,
`request_index_error`, `bridge_observed_elapsed_ms`, `bridge_observed_elapsed_reason`, `usage`,
`usage_reason`, and `usage_error` (see [result](#result)). Waiting can add `owner_process_alive`,
`owner`, and `timed_out`. List JSON has `schema_version`, `ok`, `session`, `events`, `requests`,
`unreadable_requests`, and `request_index_error`; event/request entries omit the result body and
carry the same usage fields. A record written by this version can carry `usage` or
`usage_error`; an older `tabcli` reading the same session drops them and would treat a still
journaled completion as not published, so do not downgrade `tabcli` while a session it has
written is open.

Ordinary single-result JSON can also have `question_emissions` and `question_observation`,
as described in [result](#result). These optional fields are omitted from waited and listed
results and do not alter the completion event or journal schema.

### wait

Fields: `schema_version`, `ok`, `ended`, `remaining`, and `timed_out`. `ended` is null on
timeout; otherwise it contains `address` plus the same single-result fields and waited-result
error correction as `result --request --wait --json`. `remaining` is the input-ordered array of
addresses not selected, including any already ended addresses. Timeout adds top-level `error`;
other unsuccessful selected results carry their error in `ended.error`. Argument and read errors
have `ok:false`, `ended:null`, `remaining:[]`, `timed_out:false`, and a top-level `error`.

### doctor

Fields: `schema_version`, `ok`, `session`, `provider`, `probe`, `scope`, `started_unix_ms`,
`finished_unix_ms`, `configured`, `observations`, and `checks`. Each check has `id`, `availability`,
`reason_code`, `observed_unix_ms`, `detail`, `next_action`, `evidence`, and optionally
`next_command` as an argument array. Availability is `available`, `unavailable`, or `unknown`.

The session `hold` check has reason `not_held`, `session_held`, or `hold_unreadable`; its
availability is respectively `available`, `unavailable`, or `unknown`.

`scope` has `session`, `provider`, `executable`, `executable_source`, `current_version`,
`model`, `effort`, `workspace`, `probe`, `probe_budget_ms`, and `consent_state`, as described
above. JSON object keys are serialized in sorted order (`scope` follows `schema_version`
and precedes `session`); human output prints scope first. Pi credential evidence retains
`status`, `provider`, `authType`, and `exit_code`, and adds nullable `model` and `executable`.
No command stdout, stderr, or arbitrary error text is copied into that evidence.

The session `cancel` check has reason `cancel_absent`, `cancel_recorded`, or
`cancel_unreadable`; recorded intent does not confirm interruption.

The session `origin` check has reason `origin_external`, `origin_live`, `origin_ended`,
`origin_ended_session_closed`, or `origin_unverified`. Its `evidence` holds `origin` and
`origin_observation`; only `origin_ended` has a `next_command`, the explicit close of the
diagnosed session.

### inspect --timeline

Fields: `schema_version`, `ok`, `session`, `request_id`, `session_state`, `session_error`,
`recovery_required`, `incomplete`, `unreadable_requests`, `request_index_error`, `requests`,
`entries`, `session_entries`, and `doctor_command`. Entries contain `session`, `request_id`,
`event_id`, `stage`, `observed_unix_ms`, `source`, `record_state`, and `detail`.

`requests[]` contains `result` summaries without the result body, plus `derived_from: "result"`,
`result_command` (the command to query that request as JSON), and `delivery`. `delivery` is
`"completion_recorded"` when a published completion is read, `"refused_not_sent"` when the
recorded event is Bridge's delivery refusal (`provenance: "delivery_not_sent"`, nothing was sent),
otherwise `"unknown"`. `delivery: "unknown"` does not mean that nothing was sent. A refusal
event's entry has `stage: "delivery_refusal"`; a provider completion's has `stage: "completion"`.

### search

JSON has `schema_version`, `ok`, `query`, `filters`, `limit`, `hits`, `truncated`, `incomplete`,
`incomplete_reasons`, and `scanned`. Filters contain `workspace`, `all_workspaces`, and `provider`;
scan counts contain `sessions` and `events`. Hits contain `session`, `provider`, `workspace`,
`title`, `request_id`, `event_id`, `created_unix_ms`, `excerpt`, and `result_command`. Incomplete
reasons contain `session` and `reason`. Excerpts are limited to 200 displayed characters.

### consent

Inspection has `workspace`, `current_identity`, and `record` (null if absent). A changed record has
`schema`, `identity`, `source`, and `revoked`. Identity contains `path`, `volume`, `file`,
`created`, and `owner`; non-null source evidence has `provider`, `store`, and `key`.
