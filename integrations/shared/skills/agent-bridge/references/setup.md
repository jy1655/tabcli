# Setup and recovery

## Requirements

The integration contains instructions, not an engine, daemon, provider binary, credentials,
or permission override. The **invoking host** must load this skill and permit shell execution
and reading/writing its task prompt files. Its sandbox must allow the local terminal processes
and Bridge state needed for the delegation. Respect the host's permission request or refusal;
do not change its global settings to make a command work.

The **target provider** is the CLI selected in `ask`. Install and sign in to just that CLI.
For the current engine's minimum versions and transport capabilities, consult
[providers](https://github.com/jy1655/tabcli/blob/main/docs/providers.md), and inspect the
installed engine with `tabcli doctor --provider PROVIDER --probe --json`.
Linked repository guides describe the current source. Prefer the installed `tabcli --help`
for its command contract; select the documentation's `v<installed-version>` Git tag when
checking a release-specific capability.
The probe checks local capabilities; success does not establish authentication, trust,
successful delivery, or completed work.

Agent Bridge supports macOS and native Windows, not Linux or a Linux headless container.
Windows needs `pwsh` (PowerShell 7) and a desktop session; `windows-console` is its default terminal.
macOS needs a supported terminal and its automation permissions; see
[terminals](https://github.com/jy1655/tabcli/blob/main/docs/terminals.md).
The assistant must be able to launch a real terminal on that machine.

## Missing or incompatible installation

If `tabcli` cannot be resolved, report the shell's PATH and executable lookup problem without
dumping the environment. Use a user-provided absolute executable path if available. Otherwise
point to the [installation guide](https://github.com/jy1655/tabcli#install); do not download
and execute an installer without authorization. The CLI must report `tabcli 0.4.22` or newer
and expose `ask`, `tell`, `result --request --wait`, `inspect`, `doctor`, and `close-session`.
An older version or a missing required command needs an upgrade before this workflow runs.
Plugin and engine versions do not need to match. If a newer CLI changes a command, consult
its `--help` rather than assuming the old invocation still works.

Bridge resolves target executables from absolute PATH entries. A shell alias or function that
can run `codex` is not an installed executable on PATH, and an earlier obsolete launcher can
shadow the current CLI. Use `doctor --provider PROVIDER --probe --json` to see the executable
and version Bridge actually selects before changing setup. Correct only the intended PATH
entry; putting a directory of old launchers first can change other providers too.

Upgrade through the CLI installation method the user chose, after accounting for open
sessions. An open session's hooks and helpers can retain an absolute executable path. Keep
that binary available, do not downgrade the binary used by an open session, and do not remove
old release directories that those sessions still use. Installing/updating this integration
does not install/update `tabcli` or change Bridge settings, consent, or session records.

If the provider is missing or too old, report the selected provider's doctor finding and its
installation link from the provider guide. Do not require the other three target CLIs.
Ask the user to complete that provider's own sign-in when authentication is needed; never
request, copy, or print credentials as part of a prompt.

## Workspace and shell restrictions

Use an existing project directory. A trust refusal does not authorize accepting the dialog:
the user must approve that exact workspace through the provider's own UI if they trust it.
Bridge can reuse verified existing consent using its provider-specific gates. Do not edit
trust stores, simulate approval, or add bypass flags to defeat a refusal. Once a failed
initial launch is conclusively not sent, close that task-owned session before a deliberate
new launch after the actual setup problem is fixed.

When the host refuses a shell command, surface the exact restriction and the narrow action
needed through its normal approval flow. If that environment cannot launch local terminals,
explain that the integration needs a supported desktop environment. Do not substitute a
remote daemon or an unrelated execution environment.

## Timeout, delivery, and lifecycle

Retain any session/request ids even when the shell command exits nonzero. If an interrupted
launch printed no address, use `tabcli status --workspace PATH --all --json` and inspect candidate
records to establish ownership before acting. A matching title alone is not ownership.
Never close all sessions as recovery.

`result --request REQUEST --wait` and `wait SESSION/REQUEST` only observe. Timeout leaves
the underlying request untouched. A `pending` request can still finish; `unresolved` or
`recovery_required` calls for `inspect`/`doctor`, not another prompt. `failed` and `cancelled`
are outcomes to read, not invitations to repeat the same input. A confirmed delivery refusal
can be addressed after its cause is fixed, but an uncertain delivery must never be resent
automatically. Report the provider's actual error rather than inferring success from an
empty output, process exit, or terminal text.

Cancellation and reopening are not uniform capabilities. Check the installed `tabcli --help`,
the session's doctor findings, and the matching release's provider documentation before using
either, and only do so for the user's intended operation. Acceptance of cancellation is not
completion; reopening has provider-specific ownership and receipt gates. `tell` continues an open ready
session; `ask` creates a new conversation. Do not silently replace one with the other.

A close timeout can be a partial close. Inspect the session and diagnose its recorded error;
only the normal explicit close can finish its cleanup. Never kill guessed processes or
delete session records. Closed records preserve the results for later inspection.
