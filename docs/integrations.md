# Host integrations

An integration lets a coding assistant use `tabcli` without global instructions that you write
yourself. It installs one skill, `agent-bridge`, that tells the assistant how to delegate work,
read the exact result, send follow-ups and close the sessions it created. The integration adds
no transport of its own: every launch, delivery, consent decision and close still goes through
the installed `tabcli` and its provider adapters.

The skill guides the assistant; it does not guarantee anything after the assistant stops. If the
host exits, crashes or loses context, sessions it opened may remain open or lose their owner.
Inspect their state and close task-owned sessions with `tabcli close-session SESSION --explicit`.

Version 0.4.23 was published without integrations. Release assets for the integrations start
with the next release; until then, install from a source checkout that contains `integrations/`.
In the commands below, `X.Y.Z` is a release that includes them. Recorded host checks are in
[the 2026-10-22 host integration record](verification/2026-10-22-host-integrations.md).

## Hosts and target providers

Two roles are involved, and their requirements are separate:

- The **invoking host** is the assistant you talk to. It loads the integration and runs `tabcli`
  through its shell tool.
- The **target provider** is the CLI that receives the delegated work in a managed terminal
  surface.

| Invoking host | Package | Can target |
| --- | --- | --- |
| Claude Code | Claude Code plugin with one skill | Codex, Claude Code, Agy, Pi |
| Codex | Codex plugin with one skill | Codex, Claude Code, Agy, Pi |
| Agy | Agy plugin with one skill | Codex, Claude Code, Agy, Pi |
| Pi | Pi package with one skill | Codex, Claude Code, Agy, Pi |

Any host can target any provider, including its own CLI. Install only the providers you target:
using Pi as the host to delegate to Codex needs Pi, Codex and `tabcli`, not Claude Code or Agy.
Targeting every provider does not make their capabilities identical. Cancel, reopen, follow-up
delivery and terminal support differ by provider and platform; see [providers](providers.md).

## Requirements

### Agent Bridge CLI

- `tabcli` 0.4.22 or newer on `PATH`, installed as described in the
  [README](../README.md#install). Check it with `tabcli --version`. The minimum CLI version is
  independent of the integration's version; each package records it in `integration.json`.
- macOS or native Windows. Linux, including a Linux process in WSL, is unsupported as a host
  platform too, because `tabcli` cannot run sessions there.
- On native Windows, PowerShell 7 (`pwsh.exe`) on an absolute `PATH` entry, and Windows
  Terminal's `wt.exe` for tabs. See [terminals](terminals.md#requirements-and-first-run-prompts).
- On macOS, Automation permission for the terminal that hosts the session when you use iTerm2,
  Terminal.app or Ghostty. See [macOS permissions](macos-permissions.md).

### Invoking host permissions

`tabcli` runs outside the project: it reads and writes its records under
`~/.agent-bridge/native-sessions` (or `AGENT_BRIDGE_NATIVE_STATE_DIR`), starts terminal
applications and starts the target provider's CLI. The host must let its shell tool do that.

Default restrictions stopped delegation in the recorded checks:

- Codex with the `workspace-write` sandbox was denied access to Bridge's private state before
  any session was created.
- Agy in `--print` mode loaded the skill but denied the `tabcli` command automatically.

Allow `tabcli` through the host's own approval prompt or a permission setting you choose.
Unrestricted execution modes include `codex exec --sandbox danger-full-access` and
`agy --dangerously-skip-permissions`; these let the host run any command, not only `tabcli`.
The recorded checks used them only with the user's explicit approval. The integration never
enables them, adds allow rules or changes host settings, and the skill does not pass `--yolo` to
the target provider unless you ask.

Native Windows Agy 1.4.5 also needs its native shell task kept alive when Bridge uses a console
window on a desktop without a shell window. A completed `run_command` lost the detached
target's owner even with permission bypass. The successful check used Agy's `run_command`
with `IsDaemon: true` to start `pwsh -NoLogo -NoProfile`, then `manage_task` to send commands
and inspect output on that same task. It read the initial result before composing the
follow-up, confirmed Bridge close, then exited the shell. The skill carries this procedure;
it requires those native tools to be available and does not install a background service.

### Target provider

- The provider CLI installed, at or above the minimum version in
  [providers](providers.md#versions-and-launch-options), and signed in.
- The provider executable on an absolute `PATH` entry. `tabcli` does not see shell functions or
  aliases: in the recorded checks, a `codex` that existed only as a PowerShell function failed
  the launch before a session was created.
- The provider's workspace trust resolved for the project you delegate in. Bridge reuses only
  verified existing consent for an exact workspace and never grants new trust. See
  [workspace trust and consent](security-and-data.md#workspace-trust-and-consent).

## What the skill does and does not do

The skill asks the assistant to:

- find `tabcli`, check its version and select the workspace explicitly;
- start work with `ask`, usually with `--prompt-file` and `--detach`, and continue it with
  `tell` on the returned session;
- read results with the exact session and request ids that `ask` and `tell` print;
- diagnose with `inspect --timeline`, `doctor` and `status`;
- close only the sessions it created for the task, with `close-session --explicit`.

The skill never:

- adds `--yolo` or any other permission bypass unless you ask for it;
- approves workspace trust, tool permissions or credential prompts;
- resends a prompt after a timeout or an uncertain delivery. A wait timeout is not a cancel and
  does not prove that nothing was sent; the assistant inspects the original request instead;
- closes, reuses or prunes sessions it did not create, unless you ask.

## Getting a package

### Release archives

Each release that includes integrations has four archives, each with a `.sha256` file:

| Archive | Contents of `agent-bridge-HOST-X.Y.Z/` |
| --- | --- |
| `agent-bridge-claude-X.Y.Z.zip` | `.claude-plugin/marketplace.json` and the plugin in `plugin/` |
| `agent-bridge-codex-X.Y.Z.zip` | `.agents/plugins/marketplace.json` and the plugin in `plugin/` |
| `agent-bridge-agy-X.Y.Z.zip` | The Agy plugin itself (`plugin.json`, `skills/`) |
| `agent-bridge-pi-X.Y.Z.zip` | The Pi package itself (`package.json`, `skills/`) |

The archives contain instructions only, no `tabcli` binary. Download the archive for your host
and its checksum from [GitHub Releases](https://github.com/jy1655/tabcli/releases), verify it and
extract it into a directory that you keep. On macOS:

```sh
shasum -a 256 -c agent-bridge-claude-X.Y.Z.zip.sha256
mkdir -p ~/agent-bridge-integrations
unzip agent-bridge-claude-X.Y.Z.zip -d ~/agent-bridge-integrations
```

On Windows, in PowerShell:

```powershell
$archive = "agent-bridge-claude-X.Y.Z.zip"
$expected = (Get-Content "$archive.sha256").Split()[0]
$actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch" }
Expand-Archive $archive -DestinationPath "$HOME\agent-bridge-integrations"
```

Replace `claude` with your host. The extracted directory is
`agent-bridge-integrations/agent-bridge-HOST-X.Y.Z`; the commands below call it `EXTRACTED`.
Use its absolute path.

### Source checkout

A checkout contains the same packages, generated from one source:

```text
integrations/
  shared/skills/agent-bridge/      single source for the skill and its references
  manifests/                       manifest templates, one per host
  claude/  codex/  agy/  pi/       generated packages, each with integration.json
.claude-plugin/marketplace.json    Claude Code catalog
.agents/plugins/marketplace.json   Codex catalog
```

The commands below call the checkout's absolute path `CHECKOUT`. A host that loads a package in
place from a checkout follows whatever that checkout has checked out, including unreleased
changes. Prefer a release archive or a checkout of a release tag.

Each package's `integration.json` records the package version, which equals the Cargo version,
and the oldest `tabcli` it supports. `python scripts/integrations.py check` verifies that the
generated packages match the shared source.

## Claude Code

Checked with Claude Code 2.1.308. See the [plugin documentation][claude-plugins].

Claude Code installs plugins only from a catalog. Add one of these catalogs, then install:

```sh
claude plugin marketplace add EXTRACTED      # release archive
claude plugin marketplace add CHECKOUT       # source checkout
claude plugin install agent-bridge@agent-bridge
claude plugin list
```

Adding a local catalog, installing and listing were checked from a source checkout, and the
installed version appeared in `~/.claude/plugins/cache/agent-bridge/agent-bridge/`. Claude
Code's documentation says a plugin from a catalog added by local path loads from that path, so
keep `EXTRACTED` or `CHECKOUT` in place.

To use the repository itself as the catalog, pinned to a release tag:

```sh
claude plugin marketplace add jy1655/tabcli#vX.Y.Z --sparse .claude-plugin integrations/claude
claude plugin install agent-bridge@agent-bridge
```

Without `#vX.Y.Z` the catalog follows the default branch, which can contain unreleased skill
text. This form has not been checked with this repository.

`claude plugin install` installs at user scope. `--scope project` writes the project's
`.claude/settings.json` and enables the plugin for everyone who clones it. A plugin installed
from the shell loads in the next session, or after `/reload-plugins` in an open one.

### Use

Ask in natural language, or invoke the skill explicitly:

```text
/agent-bridge:agent-bridge
```

The explicit command loaded the installed skill in the recorded check.

### Update

Catalogs outside Anthropic's own do not update automatically by default.

- **Local catalog.** Claude Code reads the catalog directory again at the next session or
  `/reload-plugins`. To move to another release, remove the catalog, which also uninstalls the
  plugin, then add the new `EXTRACTED` directory and install again.
- **Git catalog.** Refreshing the catalog and updating the plugin are separate operations:

  ```sh
  claude plugin marketplace update agent-bridge
  claude plugin update agent-bridge@agent-bridge
  ```

  The plugin updates only when the release changes its version. A catalog pinned with
  `#vX.Y.Z` does not move; change the `ref` of the `agent-bridge` entry under
  `extraKnownMarketplaces` in `~/.claude/settings.json` to select another tag.

A running session keeps the version it loaded until `/reload-plugins` or a new session.

### Remove

```sh
claude plugin uninstall agent-bridge@agent-bridge
claude plugin marketplace remove agent-bridge
```

Removing the catalog also uninstalls every plugin installed from it. The plugin contains no hooks
or executables.

## Codex

Checked with codex-cli 0.163.0. See the [plugin documentation][codex-plugins].

Add a catalog, then install and list:

```sh
codex plugin marketplace add EXTRACTED      # release archive
codex plugin marketplace add CHECKOUT       # source checkout
codex plugin add agent-bridge@agent-bridge --json
codex plugin list --marketplace agent-bridge --json
```

The source-checkout form was checked, with a separate `CODEX_HOME` and with the default one.
Codex copies the plugin into `CODEX_HOME/plugins/cache/agent-bridge/agent-bridge/VERSION/`
(`~/.codex` by default), so the installed copy does not change with the catalog directory.

To use the repository itself as the catalog, pinned to a release tag:

```sh
codex plugin marketplace add jy1655/tabcli --ref vX.Y.Z --sparse .agents/plugins --sparse integrations/codex
```

This form has not been checked with this repository.

Start a new Codex session after installing. A configuration override on a single invocation
does not load a plugin that is not installed: in the recorded check, a new `codex exec` with
`-c` overrides did not list the skill.

### Use

Ask in natural language or invoke the installed skill explicitly with `$agent-bridge`.
The recorded `codex exec` check loaded it from the installed plugin cache.

### Update

For a Git catalog, `codex plugin marketplace upgrade agent-bridge` refreshes its snapshot;
it does not by itself upgrade the installed plugin. A catalog pinned with `--ref` stays on
that ref. For a local catalog, update its source directory directly: `marketplace upgrade`
refuses local sources. Then run `codex plugin add agent-bridge@agent-bridge --json` again.
This installed a newer manifest version in a separate versioned cache in the recorded check,
preserving the unrelated user setting. Check the selected version with `plugin list` and start
a new session. Codex 0.163.0 has no separate plugin update command.

To change a catalog to a new extracted release directory or Git tag, remove this plugin and
its catalog with the commands below, add the new source with `marketplace add`, and install
again. This catalog contains only Agent Bridge; do not remove an unrelated catalog.

### Remove

```sh
codex plugin remove agent-bridge@agent-bridge --json
codex plugin marketplace remove agent-bridge
```

Both commands were checked in a separate `CODEX_HOME`; unrelated configuration was preserved.

## Agy

Checked with Agy 1.4.5. See the [plugin documentation][agy-plugins].

Agy installs this plugin from a local directory:

```sh
agy plugin validate EXTRACTED
agy plugin install EXTRACTED
agy plugin list --json
```

From a checkout, use `CHECKOUT/integrations/agy`. Agy copies the files; on Windows the recorded
check found them in `%USERPROFILE%\.gemini\config\plugins\agent-bridge`, not in the staging
path that Agy's CLI documentation names, and `agy plugin list` showed one skill. The macOS path
has not been checked. Start a new Agy session after installing.

### Use

Agy's documentation turns each skill into a slash command:

```text
/agent-bridge
```

Agy can rename a plugin skill that collides with another customization; `/skills` lists the
name it uses. In the recorded `agy --print` check, Agy loaded the installed skill.

### Update

Agy 1.4.5 has no plugin update command and no documented automatic update for these local
packages. Install the new release's directory the same way. The recorded check replaced the
installed files and preserved a disabled plugin's state without uninstalling first. Inspect
`integration.json` inside the installed path for its version; Agy's strict manifest has no
version field. `agy update` updates the Agy CLI, not this package.

### Remove

```sh
agy plugin uninstall agent-bridge
```

In the recorded 1.4.5 check, installing after an uninstall enabled the plugin again. Check
`agy plugin list --json` and set your preference with `agy plugin disable agent-bridge` or
`agy plugin enable agent-bridge` when changing installations.

## Pi

Checked with Pi 1.4.0. See the [package documentation][pi-packages].

Install the package by its absolute path:

```sh
pi install EXTRACTED
pi list
```

From a checkout, use `CHECKOUT/integrations/pi`. Pi records the path in
`~/.pi/agent/settings.json` and loads the package from it without copying, which the recorded
check confirmed; keep the directory in place. `pi install -l` writes the project's
`.pi/settings.json` instead, which Pi reads only after project trust is granted.

### Use

Ask in natural language, or force the skill:

```text
/skill:agent-bridge
```

The command works even when the `enableSkillCommands` setting hides skill commands from
completion. After installing or updating in an open session, run `/reload`.

### Update

`pi update` without a target updates Pi itself, and Pi does not update a package installed by
path: it loads what the directory holds. To change the release, install the new `EXTRACTED`
directory and remove the old entry, or replace the directory's contents. Then run `/reload` or
start a new session.

### Remove

```sh
pi remove EXTRACTED
```

Use the source exactly as `pi list` prints it. Removing the entry leaves the directory; delete
it yourself.

## Updating

Three things update independently.

1. **Release artifacts.** A release builds the CLI archives and the four integration archives
   from the same tagged commit. CI checks manifest versions, package contents and paths, and that
   every package matches the shared source. Publishing a release does not update anything
   already installed.
2. **Installed integrations.** Each host updates its own copy with the commands above, and only
   when you run them or turn on that host's automatic update. A pinned tag stays where it is.
3. **The installed CLI.** Upgrade `tabcli` as described in the [README](../README.md#install).
   The skill checks the minimum version and asks you to upgrade; it never upgrades the CLI.

When you upgrade `tabcli`:

- Finish and close the sessions your tasks created first, where you can.
- Upgrading the executable at the same path is supported: open sessions call it by path through
  their hooks and pick up the newer binary.
- Do not downgrade `tabcli`, or `AGENT_BRIDGE_EXECUTABLE`, while a session written by the newer
  version is open. Older readers drop fields and can misread a journaled completion.
- If you install the new version to a different path, keep the old executable where it is until
  the sessions it launched are closed. Do not rename, move or delete an executable that an open
  session uses.

Installing, updating or removing an integration does not install or update `tabcli` and does not
change Bridge settings, consent records or session records. There is no unified updater. A
future updater must keep the executable paths that open sessions and their hooks use, respect
session-format compatibility and never remove a binary still in use.

## Removing everything

Remove the integration from each host with its remove command above. That removes only the
skill: `tabcli`, its settings, consent records and session records stay. To remove those, close
your sessions and follow
[list, close, and remove sessions](security-and-data.md#list-close-and-remove-sessions), then
remove the executable as described in the [README](../README.md#before-you-rely-on-it).

## Verification status

Results, tested platforms, versions and settings are in
[the 2026-10-22 host integration record](verification/2026-10-22-host-integrations.md). It
separates what passed from what was not run; not every host and target combination has been
run. Checked on native Windows so far:

- every host installed its package from a source checkout and an extracted archive;
- all four explicit invocation forms loaded their skills;
- each host completed launch, exact result retrieval, a follow-up and confirmed close, covering
  Claude → Codex, Codex → Claude, Pi → Agy and Agy → Pi;
- Codex reinstallation selected a newer version while preserving unrelated settings; Agy
  replaced its files while preserving its disabled state; host removal commands were checked;
- the default Codex sandbox and Agy print-mode permissions stop delegation, as described under
  [invoking host permissions](#invoking-host-permissions).

Not yet checked:

- macOS for every host;
- installs from the repository pinned to a released tag (none yet contains the integrations).

[claude-plugins]: https://code.claude.com/docs/en/discover-plugins
[codex-plugins]: https://developers.openai.com/plugins/build/plugins
[agy-plugins]: https://www.agy.dev/docs/plugins?tab=cli
[pi-packages]: https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/packages.md
