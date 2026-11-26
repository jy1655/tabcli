# Security and data

You can inspect TAB's saved prompts and results and remove closed sessions without deleting
provider history. Those records are private, unencrypted data under your account.

TAB does not sandbox provider tools. Provider CLIs send prompts and tool data to their
configured services under your existing login. That includes Claude's messenger turn and any
result context attached to a prompt. TAB runs no hosted service and has no credential store.
It cannot attach arbitrary existing CLI sessions.

## Claude inbound messages

**Other Claude sessions on the same account can send work to a TAB-launched Claude session.
If you launch it with `--yolo`, that work runs without Claude's permission checks.** Do not
combine `--yolo` with an account whose other Claude sessions you do not control.

TAB verifies prompts sent by its own messenger, but does not filter incoming work from other
Claude sessions. Its messenger is a separate model turn: the model sees a per-request reference,
and a hook supplies your exact prompt to the official delivery tool. The receiving session
processes that prompt under its own provider settings.

## Permission modes

TAB adds the following flag only when you request `--yolo`, except that Pi's project approval
flag is also used for verified workspace consent:

| Provider | Forwarded flag | What it bypasses |
| --- | --- | --- |
| Codex | `--dangerously-bypass-approvals-and-sandbox` | Provider approvals and sandbox |
| Claude | `--dangerously-skip-permissions` | Provider permission checks |
| Agy | `--dangerously-skip-permissions` | Provider permission checks |
| Pi | `--approve` | Project approval; native tool policy stays in effect |

TAB does not add another sandbox. It does not inherit `--yolo` from the calling session or
copy it from a closed session during `reopen`. It also does not erase the provider's settings or
environment. Check those settings if you need to know the effective permission mode: omitting
`--yolo` on Claude reopen does not prove that bypass is off.

## Workspace trust and consent

Approve a workspace in the provider itself when you decide to trust it. TAB can reuse that
existing decision for the exact directory. It records the canonical path, filesystem identity,
owner, and provider that supplied the decision. Consent does not extend to parent or child
directories, and a symlink alias cannot supply fresh consent. If the original provider removes
its decision, TAB does not silently choose another provider as the source.

Git worktrees are the one exception, for every provider. When no provider trusts a directory
exactly, and Git's own files prove that it is a linked worktree registered by a repository whose
main working tree has verified consent in TAB, the worktree inherits that consent; Codex does
the same by itself. The rule is about who can register a worktree in a trusted repository: anyone
who can write that repository's `.git` directory can, and the worktree's files are then trusted
like any commit checked out in the main working tree. A cloned or fetched repository cannot
register a worktree by itself. The main working tree's own consent decides, with its source,
identity and revocation: `tabcli consent revoke` on the main working tree also stops its
worktrees. A provider that declines the worktree itself, either the source of that consent or
the provider being launched, stops it too.

TAB reads the worktree's `.git` file and the registration it names without running `git`. Before
the worktree check touches the main working tree that the file names, TAB asks each provider
whether its own store lists exactly that path as trusted, answered from the store alone, with no
I/O on the path or its ancestors and no `git`; the worktree check never touches a path that no
store lists. That describes the worktree check only: assessing the launch folder itself as an
exact folder still runs each provider's reader there as 0.4.49 does, and Claude's reader runs
`git rev-parse` in the launch folder when a repository encloses it, which follows the folder's
own `.git` pointer (a known follow-up). A path that steps back with `..` over a name it named is
refused before any I/O. TAB then checks the registration in both directions by file identity
(the back-pointer must name `.git` in the launch folder itself, so a hard link elsewhere does not
count; it and the registration's common directory are first compared as Git spells them, so a
stale registration that points into a third folder is refused before TAB looks there), absolute
and relative registrations (`git worktree add --relative-paths`) alike, and
refuses a path through a symbolic link or junction, a submodule, a bare repository, and a main
working tree whose `.git` points to a common directory of its own. The check defends this rule
against whatever the launch folder holds and against any local user who cannot write the main
working tree's `.git` or an ancestor directory of either tree; it does not defend against
someone who can replace those ancestors while it runs, just as an exactly trusted folder is
trusted by its path. A registration that still spells the worktree's old path, including a
rename that changed only letter case on a case-insensitive volume, does not inherit, and the
provider's own trust dialog applies: `git worktree repair <worktree>` rewrites the registration
of a moved worktree, and `git -c core.ignorecase=false worktree repair <worktree>` also one that
differs only by case, which Git otherwise keeps on such a volume. The screen is a necessary
condition only, never consent: after these checks
the main working tree's own consent is assessed as for any exact folder, and a path that a store
lists but its provider's reader refuses gives no inheritance. TAB does not evaluate Git
configuration, `HEAD`, objects or refs: a repository whose configuration moves its working tree
(`core.worktree`) or uses unusual extensions is judged by its registration alone. The record
then also names the main working tree (`repository_root`).

TAB checks consent again at every use, not only at launch, and consumes it at the act: when a
provider option is planned and again for that command immediately before the provider starts,
immediately before a trust-dialog key, and immediately before every Agy paste is handed to the
terminal; `doctor` reports the same check. That is the last point TAB's own code controls, and a
change after it takes effect for the next act, not the one in flight: a revocation that lands
while the terminal already carries out a paste does not stop that paste. The final check before
a provider starts reads no consent for a command that carries no trust option, as before; every
`ask` and supported `reopen` still assesses consent while the session is resolved. Each check
reads the records, provider stores and Git files as they are now and writes nothing. Provider or
Git evidence it cannot read gives no trust option and no dialog key, as missing consent did
before; `doctor` and the Agy paste gate report it as unreadable, and a command that already
carries a trust option does not start. TAB's own consent record that cannot be read or decoded
is an error, as before. A session's check needs the source recorded at its
start: after `tabcli consent reset` of the workspace or of its main working tree, only a new
`ask` or a supported `reopen` acquires consent again. What a launch recorded is history. A revoked consent, a source that no longer trusts the directory or a worktree
that is no longer registered stops a TAB paste into a running Agy session that relies on its
session-only setting. It does not change what the running Agy trusts, and a session whose
workspace Agy's own trust store lists exactly keeps its pastes.

TAB never writes a provider trust store. With verified consent, Codex receives a process-local
configuration override, Pi receives its one-run approval option, and Agy 1.5.17 or newer receives
a settings file for that session only (`--settings`, which Agy documents as never saved), so no
key is sent and Agy's own store is not written. Only Agy 1.5.17 on macOS has been measured
(standalone, and through TAB on Herdr in a new worktree): no trust dialog, and its store
unchanged. For an ordinary folder that only another provider trusts, this changes what 0.4.49
did with Agy: in iTerm2, Terminal.app and the native Windows console TAB used to answer Agy's
dialog, after which Agy saved the folder in its own store; in Herdr, tmux and WezTerm the launch
failed because no guarded key can be sent there; in Ghostty and Warp it failed at the screen
read. Now all of them launch with the setting, and Agy no longer saves such a folder because of
TAB. Without such an option, and only with exact consent, TAB can answer Claude's startup trust
dialog, or that of an Agy whose version the launch identified as older than 1.5.17, once in
iTerm2, Terminal.app, and the native Windows console; the provider saves the decision after the
guarded response. An Agy launch whose version or consent check was inconclusive gets no key. See
[terminal requirements and first-run prompts](terminals.md#requirements-and-first-run-prompts)
for the terminal-specific limits.

Before responding, TAB checks that it owns the surface and checks the captured screen again.
It then checks that the provider saved its decision. TAB never answers a dialog for consent that
a worktree inherits, because the provider would then save the worktree as trusted. Without
verified consent, the dialog stays for you to answer.

To see the recorded source for a workspace, run:

```sh
tabcli consent inspect /path/to/project
```

Use `tabcli consent revoke /path/to/project` to stop sharing that decision. This leaves
provider-owned trust unchanged. Use `tabcli consent reset /path/to/project` when you want
a later launch to assess fresh evidence; reset clears TAB's source and revocation decision,
but does not approve the workspace. Shared records live in `workspace-consent/` under the state
root. Each session keeps its own assessment in `workspace-consent.json`; a reopened session
assesses its workspace for itself, like a fresh `ask`, and inherits nothing from the source
session's assessment.

If consent is missing or unreadable, check the store TAB reads. In these paths, home means
`HOME` when set, otherwise `USERPROFILE`:

- Codex: `config.toml` under `CODEX_HOME`, or `<home>/.codex/config.toml` when unset.
- Claude: `.claude.json` under `CLAUDE_CONFIG_DIR`, or `<home>/.claude.json` when it is unset or
  empty (an empty value is unset, as it always was for the session registry; through 0.4.12 the
  trust store read it as the relative `.claude.json`). The session registry that `reopen` reads is
  `sessions/` under the same directory, or `<home>/.claude/sessions` when it is unset or empty.
- Agy: `<home>/.gemini/antigravity-cli/settings.json`.
- Pi: `trust.json` under `PI_CODING_AGENT_DIR`, or `<home>/.pi/agent/trust.json` when unset.

Some initial prompts wait rather than risk typing a submission key onto a trust dialog:

- Codex on native Windows waits for exact trust, verified shared consent, or its empty composer.
  An empty screen or missing dialog text is not enough.
- Agy on native Windows waits for its exact trust entry, or the session-only setting TAB passed
  at launch while consent still holds, and this session's customization-load log entry, then
  startup readiness. On macOS and Linux its initial prompt is an argument and can run behind the
  dialog. Every follow-up, on Windows too, waits for the same trust and log evidence before
  input. The setting alone is not enough: if Agy ignored it, the log entry is missing and
  nothing is pasted.
- Pi on native Windows waits for the current turn's startup event after the trust procedure.
  That event can follow a decline; it is not shared consent.
- Claude on native Windows delivers the initial prompt through official cross-session messaging
  rather than typing it into the console.

A trust wait stops when the session ends or the deadline expires. After resolving a trust dialog,
retry a withheld macOS Agy follow-up with `tell` only if the session is `ready`. If a Windows
initial request failed, close that session and run a new `ask`; a failed session cannot take
`tell`. The [provider page](providers.md) gives the recovery steps for each CLI.

If delivery is uncertain, TAB keeps the claim and does not resend. It refuses another `tell`
while the earlier request remains claimed. Inspect that request rather than repeating its prompt;
follow the [provider-specific recovery instructions](providers.md).

## Local records

Unless you set `AGENT_BRIDGE_NATIVE_STATE_DIR`, TAB stores records at:

| Platform | State root |
| --- | --- |
| macOS | `$HOME/.agent-bridge/native-sessions` |
| Windows, with `HOME` set | `$env:HOME\.agent-bridge\native-sessions` |
| Windows, with `HOME` unset | `$env:USERPROFILE\.agent-bridge\native-sessions` |

The code uses `HOME` first, then `USERPROFILE`, on both platforms. The override replaces the
entire root. `settings.json` in that root holds TAB's surface-creation preferences.

Inside a session directory, these are the records most useful when checking a request. Some
exist only during a turn or after close; `agy.log` and `agy-session-trust.json` are specific to Agy.

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
├── agy-session-trust.json Agy's session-only trust of the workspace, when applied
└── closed.json            Recorded closed status
```

For launch failure details, read `status.json` and `launch.log` alongside `launch.json` and the
claim. The launch receipt has no failed or cancelled phase. `initial-prompt.txt` can be removed
after delivery; its absence does not mean the session had no prompt.

These records contain plain text or JSON. Follow-up payloads, hook settings, correlation data,
and other logs also live in provider-specific records. Prompts and results can contain secrets
from your input or the provider's work; do not publish a session directory without reviewing
it. The [contributor record map](architecture.md#session-records) lists the lifecycle records.

Pi on native Windows also retains `pi-input-accepted-<claim>.json`. It contains the claim,
Pi session id, submitted frame's SHA-256 and UTF-8 byte length, and the `agent_start` signal.
The pending Pi record holds the expected fingerprint. These records contain no prompt copy
and grant no result, input, or cancellation authority. Close retains them with the session.

For Codex, `codex-questions-<claim>.json` retains structured question ids, titles, and options,
Request identity, and an incompleteness flag. The complete serialized record is limited to
64 KiB. It uses the same private record writer and remains after completion and close until
the session records are removed. Question strings can contain sensitive data. The record
contains no copied prompt, transcript, transcript path, timestamps, or tool output. Queries
validate it against the selected Receipt without opening provider history or writing records.
A missing record is unknown, and an unreadable record changes no result or delivery authority.

TAB reads its own JSON records (`manifest.json`, `status.json`, `closed.json`,
`turn.completion.json`, `launch.json` and the receipts under `requests/`) through one decoder
per record, in every command. The decoder takes the bytes as strict UTF-8, so a record with
invalid UTF-8 is unreadable rather than read with replacement characters. A `*_unix_ms` value
above the JSON unsigned integer range, anywhere in the record, also makes it unreadable, so
that a command rendering the record cannot fail while serializing it. `status.json` and
`closed.json` are read as regular files only, as the manifest, the journal, the launch record
and the receipts already were: a symbolic link or a non-file at either path is refused with
`refusing non-regular session file`, not followed. Missing records keep each command's own
meaning (a missing `closed.json` is an open session; a missing `status.json` fails the read
that requires it).

The `events/` directory is held to the same rule. Every command that lists it refuses a
symbolic link (on Windows, also a junction) or a non-directory at `events` with an error
(`events directory is a symlink`, `events is not a directory`) instead of reading the results
behind it: `result`, `inspect` and `wait` fail, `status` reports the session as incomplete with
that reason, and `sessions` shows `"results": 0` for it. A missing `events/` lists no results.

TAB makes private directories mode `0700` and private records mode `0600` on Unix. On Windows,
it installs a protected access list giving the current user full control, with inheritance for
private directories. Administrators and the operating system can still access the data.

Before accepting a trust store as evidence, TAB checks who can change it. On Unix the current
user must own it and other accounts must not have write access. On Windows it reads ownership
and the access list from the same handle as the content. The owner must be the token user or
default owner. No other account may write, append, or change ownership or the access list, except
SYSTEM, Administrators, and OWNER RIGHTS. A missing or unrecognized access list is refused.
This checks current access; it cannot establish who wrote the content earlier. The check does
not exclude a process that already has the store open for writing. If a store fails this check,
TAB does not use it to approve workspace input.

Provider transcripts, credentials, and settings are separate. Deleting TAB's root does not
delete them or sign you out of the provider.

## List, close, and remove sessions

List sessions before choosing which to close:

```sh
tabcli sessions
tabcli close-session session-example --explicit
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
tabcli prune-sessions --closed-before-days 30 --explicit
```

The day count must be positive. Prune requires both closed status and a closed tombstone older
than the cutoff. It skips sessions with active surface handles, pending claims or completions,
or owner records that prevent it from proving removal is safe. Legacy resume records also block
removal. Prune does not close active sessions for you.

Some transient delivery records are removed as turns settle, so retained history does not include
every intermediate delivery artifact.

To remove all TAB records, first close every session in the root you intend to delete. For
the default macOS root, run this only after those closes succeed:

```sh
rm -rf -- "$HOME/.agent-bridge/native-sessions"
```

For the default Windows root when `HOME` is unset, the PowerShell command is:

```powershell
Remove-Item -LiteralPath "$env:USERPROFILE\.agent-bridge\native-sessions" -Recurse
```

If `HOME` or `AGENT_BRIDGE_NATIVE_STATE_DIR` selects another root, use that exact path instead.
Remove additional roots separately. Inspect `~/.agent-bridge` for any legacy TAB data before
removing that parent directory; current commands do not read or delete legacy data automatically.
Provider history needs the provider's own removal procedure. Removing the executable is separate
from deleting records.

Report a suspected vulnerability using [the security reporting policy](../SECURITY.md).

## Sharing diagnostic summaries

The offline [diagnostic formatter](cli.md#offline-diagnostic-summary) projects saved public
query JSON onto an explicit allowlist. It does not redact arbitrary text with regular
expressions. Strings under numeric, boolean, enum or identifier-looking keys are not trusted:
numeric and boolean fields require their exact JSON types, enums require a known complete
literal, version strings require a bounded complete numeric form, and all identifiers are
omitted. Timeline filenames are projected only to fixed `requests/*.json`/`events/*` families;
unsupported path shapes are omitted. Question-observation qualifiers retain only the exact
`incomplete` and `unreadable` categories, with no question or option text.
Unknown keys are never copied. Terminal controls, bidi text and Markdown instructions
in omitted content therefore do not reach the output. Original JSON and Result bytes are
not rewritten, and the formatter does not open provider-private records.

This is a deliberately lossy support aid, not an anonymization or authenticity guarantee.
Validated categories, counts, booleans and numeric versions still disclose those facts; a
fabricated input can put plausible but false values in those fields. The formatter cannot
establish who produced the JSON, whether records are complete, or whether runtime behavior
matches it. Review the projection for your sharing context. Share only the projected stdout
when appropriate; the original export and session directory can still contain secrets.
