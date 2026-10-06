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
| `claude` | 2.1.234 | `--model` | `--effort` | `--dangerously-skip-permissions` |
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
agent-bridge ask codex --workspace /path/to/project --prompt "Explain the project structure."
```

Replace `codex` with `claude`, `agy`, or `pi`. Install and sign in to that CLI first; the sections
below explain its workspace-trust requirements and delivery limits.

## Codex

Install and sign in to `codex`. You need 0.147.0 or newer to launch, and 0.149.0 or newer for
follow-ups. Open the project in Codex first and resolve its workspace-trust prompt.

On macOS, Bridge passes the initial prompt as a launch argument. On native Windows, it waits for
Codex's startup delay and either verified workspace trust or the empty composer before typing.
If that wait expires, the error says `workspace trust is not verified` and that no initial input
was sent. Check the managed surface and approve the workspace if you trust it. After a failed
initial request, close that session and run a new `ask`; `tell` requires a ready session.
An empty screen does not count as a ready composer.

`tell` uses `codex queue --thread <UUID> --message <TEXT>` to address the same conversation.
Codex chooses a shared or embedded server; you do not need to start a shared daemon. Bridge waits
for Codex's exact queue acceptance, then for the turn's completion. Acceptance alone is not a
result.

With Codex older than 0.149.0, `tell` fails before anything is typed and restores the session to
`ready`. Upgrade Codex and launch a new session. A follow-up also needs the thread UUID from the
first completed turn. If the queue is unavailable, run `agent-bridge doctor <session> --probe`
and inspect the reported prerequisite. Bridge never falls back to typing a follow-up: the
surface could be showing another thread or a picker.

Codex reports finished turns through its `notify` hook. Bridge accepts an assistant result ending
with the pending marker, or a completion whose last string input begins with Bridge's delegation
header and ends with that marker. It recognizes Codex's IDE-context wrapper. These checks identify
framing, not provenance: a copied framed prompt can still match. The first accepted result
establishes the thread, and later results must name that thread.

IDE-wrapper support comes from Codex's source and Bridge's tests. A live session with `/ide`
is not verified; see the [Windows verification record][win10].

`reopen` refuses closed Codex sessions. Bridge still needs checks for the thread's writer lock
and queued inputs before supporting resume; start a new session instead.

## Claude Code

Install and sign in to `claude` 2.1.234 or newer. Resolve workspace trust in Claude before launch,
or use [existing workspace consent](security-and-data.md#workspace-trust-and-consent).

The macOS initial prompt is a launch argument. On native Windows, Bridge delivers it after launch
through Claude's official cross-session messaging. Every follow-up uses that messaging path too:
a separate, nonpersistent Claude messenger model turn calls `ListAgents` to find your session
and `SendMessage` to deliver the prompt. Bridge removes inherited Claude session markers so a
launch from inside Claude does not create a nested session with an undiscoverable inbox.

Messaging must be available in the running CLI's backend, features, and settings, and discovery
must find the exact live local session. A recent version or saved inbound setting does not prove
those conditions. If delivery fails, run `agent-bridge doctor <session>` and read the messaging
checks. Bridge does not substitute terminal input.

A proven not-sent follow-up releases its claim and restores `ready`. A proven not-sent Windows
initial delivery leaves the session `failed`; close it and start a new session. An uncertain
delivery keeps a still-pending request claimed. The initial-delivery error says
`could not be confirmed`; the follow-up error says `could not confirm delivery`. Do not repeat
that prompt. Use `agent-bridge result <session> --request <request-id>` to inspect the original
request, or explicitly close the session if you decide to abandon it.

The messenger model sees a per-request reference, not your follow-up text. A `PreToolUse` hook
supplies the exact addressed payload through `updatedInput`, and a `PostToolUse` report confirms
what ran. Claude's `Stop` hook reports the finished result. Cross-session turns carry a
per-request marker, while an argument-delivered initial turn has its own hook path. Stored
results retain Claude's conversation identity.

Bridge records `StopFailure` for an argument-delivered initial turn. For a pending cross-session
turn, it cannot identify which request failed and leaves the claim held. This affects every
follow-up and the Windows initial prompt. Inspect the original request; do not resend it merely
because Claude reported an error.

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

## Agy

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

`reopen` refuses Agy sessions. Its presence records do not prove who still holds a conversation,
and Bridge's transcript reader follows only a newly created conversation. Start a new session.

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
Pi's session and turn identity. Provider errors are recorded too; a separate failure signal
preserves an error when the extension cannot deliver the result to Bridge.

`reopen` refuses Pi sessions because Bridge cannot verify whether a live process still holds
the conversation. Start a new session instead.

## Verification boundary

The delivery paths above are implemented for macOS and native Windows. Automated adapter and
terminal tests cover transport choices, correlation, and refusals; they do not authenticate to
model services. This table lists authenticated live evidence and its limits. Unlisted
provider/terminal combinations are not verified.

| Provider/platform | Live evidence | Limitations |
| --- | --- | --- |
| Codex/macOS | [2026-10-06][mac12] | Listed terminals only; uses `--yolo` |
| Claude/macOS | [2026-10-06][mac12] | WezTerm; uses `--yolo`; no reopen |
| Agy/macOS | [2026-10-04][mac11] | WezTerm; provider defaults |
| Pi/macOS | [2026-09-24 iTerm2 run][old7] | Recent WezTerm attempt failed authentication |
| Codex/Windows | [2026-10-02][win10] | Older authenticated evidence |
| Claude/Windows | [2026-10-02][win10], [reopen][old7] | Reopen evidence dates to 2026-09-24 |
| Agy/Windows | [2026-10-02][win10] | Older authenticated evidence |
| Pi/Windows | [2026-10-02][win10] | Older authenticated evidence |
| All/Linux | Not verified | Refusal tests only; no visible-session transport |

The latest Mac record covers Codex in WezTerm, Terminal.app, iTerm2, and Ghostty, and Claude in
WezTerm. Those runs used `--yolo`; the earlier Mac record covers Codex, Claude, and Agy with
provider defaults. Pi's recent WezTerm attempt failed authentication and is not a round-trip pass.
The Windows authenticated evidence predates the current release. The newer
[daemon-absent Codex queue probe][queue] used a mock model, not an authenticated TUI.

Warp requires a reachable official Control endpoint and provable surface ownership. It cannot
submit terminal input, so Agy and Pi follow-ups are unsupported there; choose another terminal
for those providers. Warp has automated tests. The [0.1.1 check][mac11] returned an empty Warp
instance list; Warp was not exercised for [0.1.2][mac12]. Linux is unsupported even where shared
code compiles. See [terminal support](terminals.md) for surface-specific constraints.

[mac12]: verification/2026-10-06-macos-0.1.2.md
[mac11]: verification/2026-10-04-macos-0.1.1.md
[win10]: verification/2026-10-02-windows.md
[old7]: releases/0.0.7.md
[queue]: verification/2026-10-04-codex-queue.md
