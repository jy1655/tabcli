# Security and data

You can inspect Bridge's saved prompts and results and remove closed sessions without deleting
provider history. Those records are private, unencrypted data under your account.

Bridge does not sandbox provider tools. Provider CLIs send prompts and tool data to their
configured services under your existing login. That includes Claude's messenger turn and any
result context attached to a prompt. Bridge runs no hosted service and has no credential store.
It cannot attach arbitrary existing CLI sessions.

## Claude inbound messages

**Other Claude sessions on the same account can send work to a Bridge-launched Claude session.
If you launch it with `--yolo`, that work runs without Claude's permission checks.** Do not
combine `--yolo` with an account whose other Claude sessions you do not control.

Bridge verifies prompts sent by its own messenger, but does not filter incoming work from other
Claude sessions. Its messenger is a separate model turn: the model sees a per-request reference,
and a hook supplies your exact prompt to the official delivery tool. The receiving session
processes that prompt under its own provider settings.

## Permission modes

Bridge adds the following flag only when you request `--yolo`, except that Pi's project approval
flag is also used for verified workspace consent:

| Provider | Forwarded flag | What it bypasses |
| --- | --- | --- |
| Codex | `--dangerously-bypass-approvals-and-sandbox` | Provider approvals and sandbox |
| Claude | `--dangerously-skip-permissions` | Provider permission checks |
| Agy | `--dangerously-skip-permissions` | Provider permission checks |
| Pi | `--approve` | Project approval; native tool policy stays in effect |

Bridge does not add another sandbox. It does not inherit `--yolo` from the calling session or
copy it from a closed session during `reopen`. It also does not erase the provider's settings or
environment. Check those settings if you need to know the effective permission mode: omitting
`--yolo` on Claude reopen does not prove that bypass is off.

## Workspace trust and consent

Approve a workspace in the provider itself when you decide to trust it. Bridge can reuse that
existing decision for the exact directory. It records the canonical path, filesystem identity,
owner, and provider that supplied the decision. Consent does not extend to parent or child
directories, and a symlink alias cannot supply fresh consent. If the original provider removes
its decision, Bridge does not silently choose another provider as the source.

With verified existing consent, Bridge can answer Claude or Agy startup trust dialogs once in
iTerm2, Terminal.app, and the native Windows console. In Ghostty, WezTerm, or Warp, approve the
workspace in the provider before launching through Bridge. Consent alone does not enable a
guarded response in those terminals.

Before responding, Bridge checks that it owns the surface and checks the captured screen again.
It then checks that the provider saved its decision. Without verified consent, the dialog stays
for you to answer. Bridge never writes a provider trust store directly; the provider saves the
decision after the guarded response. Codex instead receives a process-local configuration
override, and Pi receives its one-run approval option.

To see the recorded source for a workspace, run:

```sh
agent-bridge consent inspect /path/to/project
```

Use `agent-bridge consent revoke /path/to/project` to stop sharing that decision. This leaves
provider-owned trust unchanged. Use `agent-bridge consent reset /path/to/project` when you want
a later launch to assess fresh evidence; reset clears Bridge's source and revocation decision,
but does not approve the workspace. Shared records live in `workspace-consent/` under the state
root. Each session keeps its own assessment in `workspace-consent.json`.

If consent is missing or unreadable, check the store Bridge reads. In these paths, home means
`HOME` when set, otherwise `USERPROFILE`:

- Codex: `config.toml` under `CODEX_HOME`, or `<home>/.codex/config.toml` when unset.
- Claude: `.claude.json` under `CLAUDE_CONFIG_DIR`, or `<home>/.claude.json` when unset.
- Agy: `<home>/.gemini/antigravity-cli/settings.json`.
- Pi: `trust.json` under `PI_CODING_AGENT_DIR`, or `<home>/.pi/agent/trust.json` when unset.

Some initial prompts wait rather than risk typing a submission key onto a trust dialog:

- Codex on native Windows waits for exact trust, verified shared consent, or its empty composer.
  An empty screen or missing dialog text is not enough.
- Agy on native Windows waits for its exact trust entry and this session's customization-load
  log entry, then startup readiness. On macOS its initial prompt is an argument and can run
  behind the dialog; follow-ups wait for the trust and log evidence before input.
- Pi on native Windows waits for the current turn's startup event after the trust procedure.
  That event can follow a decline; it is not shared consent.
- Claude on native Windows delivers the initial prompt through official cross-session messaging
  rather than typing it into the console.

A trust wait stops when the session ends or the deadline expires. After resolving a trust dialog,
retry a withheld macOS Agy follow-up with `tell` only if the session is `ready`. If a Windows
initial request failed, close that session and run a new `ask`; a failed session cannot take
`tell`. The [provider page](providers.md) gives the recovery steps for each CLI.

If delivery is uncertain, Bridge keeps the claim and does not resend. It refuses another `tell`
while the earlier request remains claimed. Inspect that request rather than repeating its prompt;
follow the [provider-specific recovery instructions](providers.md).

## Local records

Unless you set `AGENT_BRIDGE_NATIVE_STATE_DIR`, Bridge stores records at:

| Platform | State root |
| --- | --- |
| macOS | `$HOME/.agent-bridge/native-sessions` |
| Windows, with `HOME` set | `$env:HOME\.agent-bridge\native-sessions` |
| Windows, with `HOME` unset | `$env:USERPROFILE\.agent-bridge\native-sessions` |

The code uses `HOME` first, then `USERPROFILE`, on both platforms. The override replaces the
entire root. `settings.json` in that root holds Bridge's surface-creation preferences.

Inside a session directory, these are the records most useful when checking a request. Some
exist only during a turn or after close; `agy.log` is specific to Agy.

```text
session-example/
├── manifest.json          Provider, workspace, and launch choices
├── initial-prompt.txt     Initial prompt as retained for delivery
├── status.json            Recorded state and error reason
├── launch.json            Launch claim, deadline, and spawn phase
├── launch.log             Launch stages, errors, and exit codes
├── native-session.json    Owner identity used to check the surface
├── terminal.json          The bound surface
├── workspace-consent.json Workspace trust source and application
├── requests/              Receipts connecting requests to results
├── events/                Recorded turn results
├── agy.log                Agy readiness, input receipts, and errors
└── closed.json            Recorded closed status
```

For launch failure details, read `status.json` and `launch.log` alongside `launch.json` and the
claim. The launch receipt has no failed or cancelled phase. `initial-prompt.txt` can be removed
after delivery; its absence does not mean the session had no prompt.

These records contain plain text or JSON. Follow-up payloads, hook settings, correlation data,
and other logs also live in provider-specific records. Prompts and results can contain secrets
from your input or the provider's work; do not publish a session directory without reviewing
it. The [contributor record map](architecture.md#session-records) lists the lifecycle records.

Bridge makes private directories mode `0700` and private records mode `0600` on Unix. On Windows,
it installs a protected access list giving the current user full control, with inheritance for
private directories. Administrators and the operating system can still access the data.

Before accepting a trust store as evidence, Bridge checks who can change it. On Unix the current
user must own it and other accounts must not have write access. On Windows it reads ownership
and the access list from the same handle as the content. The owner must be the token user or
default owner. No other account may write, append, or change ownership or the access list, except
SYSTEM, Administrators, and OWNER RIGHTS. A missing or unrecognized access list is refused.
This checks current access; it cannot establish who wrote the content earlier. The check does
not exclude a process that already has the store open for writing. If a store fails this check,
Bridge does not use it to approve workspace input.

Provider transcripts, credentials, and settings are separate. Deleting Bridge's root does not
delete them or sign you out of the provider.

## List, close, and remove sessions

List sessions before choosing which to close:

```sh
agent-bridge sessions
agent-bridge close-session session-example --explicit
```

Replace `session-example` with an id from the list. Close checks ownership and ends that session's
surface; it retains the records. If close refuses because ownership cannot be established,
inspect the session and surface before removing any records. The public `sessions` command also
repairs unfinished lifecycle changes and dead owners; it is not a read-only query.

Use `inspect`, its timeline, `result`, `search`, or `doctor` when you only want to observe. They do
not write, repair, resend, close, or publish pending completions. The internal read-only sessions
query follows the same rule; the public `sessions` command performs repair. See the
[CLI reference](cli.md) for the output and options of each command.

There is no automatic age-based pruning. To delete eligible closed session directories older
than thirty days, run:

```sh
agent-bridge prune-sessions --closed-before-days 30 --explicit
```

The day count must be positive. Prune requires both closed status and a closed tombstone older
than the cutoff. It skips sessions with active surface handles, pending claims or completions,
or owner records that prevent it from proving removal is safe. Legacy resume records also block
removal. Prune does not close active sessions for you.

Some transient delivery records are removed as turns settle, so retained history does not include
every intermediate delivery artifact.

To remove all Bridge records, first close every session in the root you intend to delete. For
the default macOS root, run this only after those closes succeed:

```sh
rm -rf -- "$HOME/.agent-bridge/native-sessions"
```

For the default Windows root when `HOME` is unset, the PowerShell command is:

```powershell
Remove-Item -LiteralPath "$env:USERPROFILE\.agent-bridge\native-sessions" -Recurse
```

If `HOME` or `AGENT_BRIDGE_NATIVE_STATE_DIR` selects another root, use that exact path instead.
Remove additional roots separately. Inspect `~/.agent-bridge` for any legacy Bridge data before
removing that parent directory; current commands do not read or delete legacy data automatically.
Provider history needs the provider's own removal procedure. Removing the executable is separate
from deleting records.

Report a suspected vulnerability using [the security reporting policy](../SECURITY.md).
