# Terminal Agent Bridge (TAB)

[한국어](README.ko.md) follows this README; this one is the reference when they differ.

Use `tabcli` to launch the Codex, Claude Code, Agy, and Pi CLIs you already have installed and
signed in to, in terminal tabs or windows you can watch. Read the result, send follow-up work to the
same conversation, and explicitly close the session when you finish. TAB runs on macOS and native
Windows; Linux, including Linux processes in WSL, is unsupported, and Warp support is limited.

## Try a session

After [installing `tabcli`](#install) and signing in to Codex 0.149.0 or newer, run this from your
project directory. Scripts and other agents can use `tabcli` while you watch the CLI's conversation
and tool activity. Each CLI keeps its own UI and settings.
The example uses provider defaults. The ids and results below are illustrative; use your own ids.

```sh
tabcli ask codex --prompt "Where does this program start?"
```

```text
session: session-K7m2Qx
request: request-1791285000000000000-4217-0

The program starts in src/main.rs.
```

`ask` waits for the result. The session stays open so you can continue the same conversation:

```sh
tabcli tell session-K7m2Qx --prompt "Which function parses its arguments?"
```

```text
session: session-K7m2Qx
request: request-1791285060000000000-4281-0

parse_args_from parses the command-line arguments.
```

Close the session when you finish; its recorded results remain available.

```sh
tabcli close-session session-K7m2Qx --explicit
```

```text
closed session-K7m2Qx
```

If close fails, inspect the reported session and run `tabcli doctor SESSION` before retrying;
follow the [close recovery instructions](docs/cli.md#close-session).

A timeout while waiting for a result does not cancel work already delivered. A startup timeout can
prevent launch. Inspect the original request before retrying; follow the
[request recovery instructions](docs/cli.md#shared-prompt-and-output-options).

If launch fails, run `tabcli doctor --provider codex --probe`. For an existing session, run
`tabcli doctor SESSION` and `tabcli inspect SESSION --timeline`, replacing `SESSION` with its id;
see [diagnostics](docs/cli.md#doctor).

## Support

These tables summarize selected historical runs. The linked records identify the tested versions,
settings, and limitations. These are not checks of every combination on version 0.2.5.

| Platform / terminal | Status | Recorded live evidence |
| --- | --- | --- |
| macOS / iTerm2 | Implemented | Codex, 2026-10-07 |
| macOS / Terminal.app | Implemented | Codex, 2026-10-07 |
| macOS / Ghostty | Implemented | Codex, 2026-10-07 |
| macOS / WezTerm | Implemented | Codex/Claude/Agy, 2026-10-07 |
| macOS / Warp | Limited | Not verified |
| Native Windows | Implemented | All four providers, 2026-10-02 |
| Windows Terminal tab | Preferred surface | Tab creation/follow-up/close, 2026-10-01 |
| Windows console window | Fallback | Initial prompt/close, 2026-10-01 |
| Linux, including a Linux process in WSL | Unsupported | No session transport |

macOS detects the invoking terminal and uses Terminal.app when it cannot identify the host.
Terminal.app always opens a new window. Windows prefers a tab in a dedicated Windows Terminal
window and uses a console window when a tab cannot be created. See
[terminal selection and settings](docs/terminals.md#selection-and-settings), including
`settings macos-open-mode` and `settings windows-tab-window`.

Warp requires an authorized official Control endpoint and the relevant creation features.
It cannot submit terminal input, so Agy and Pi follow-ups are unsupported there. Its authenticated
round trip and close are not verified. Choose another terminal for those providers.

| Provider CLI | Minimum launch version |
| --- | --- |
| Codex (`codex`) | 0.147.0 |
| Claude Code (`claude`) | 2.1.234 |
| Agy (`agy`) | 1.1.12 |
| Pi (`pi`) | 0.84.1 |

Codex follow-ups require 0.149.0 or newer; no shared daemon is required. Claude needs working
cross-session messaging in its installed backend and configuration, not only a recent version.
See [provider requirements](docs/providers.md) for delivery paths and failure handling.

- [2026-10-07 macOS, 0.2.5](docs/verification/2026-10-07-macos-0.2.5.md): Codex, Claude, Agy
  and Pi in iTerm2; both requests and cleanup passed.
- [2026-10-07 macOS, 0.2.4](docs/verification/2026-10-07-macos-0.2.4.md): Codex in four
  terminals and Claude, Agy, Pi in WezTerm; Pi missing-credential timeout diagnostics.
- [2026-10-07 macOS, 0.2.3](docs/verification/2026-10-07-macos-0.2.3.md): Codex in iTerm2, Pi in
  WezTerm with a logged-in provider and with a provider that has no credentials (the #83
  reason), and Claude and Agy in WezTerm; Warp was not run.
- [2026-10-07 macOS, 0.2.2](docs/verification/2026-10-07-macos-0.2.2.md): Codex in the four
  terminals above, Claude in WezTerm, Agy in WezTerm in its default permission mode and with
  `--yolo`, and Pi in WezTerm with a logged-in provider, all with the screen unlocked.
- [2026-10-07 macOS, 0.2.1](docs/verification/2026-10-07-macos-0.2.1.md): Codex in the four terminals
  above and Claude in WezTerm with provider defaults; Agy in WezTerm with `--yolo`. With the
  screen locked, Terminal.app and Ghostty did not complete a round trip. Agy's default-mode
  attempt and Pi's attempt timed out; they were not round-trip passes.
- [2026-10-06 macOS](docs/verification/2026-10-06-macos-0.1.2.md): Codex in the four terminals
  above and Claude in WezTerm, using `--yolo`, not default approval modes.
- [2026-10-04 macOS](docs/verification/2026-10-04-macos-0.1.1.md): Codex, Claude, and Agy with
  provider defaults. Pi's WezTerm attempt failed authentication; it was not a round-trip pass.
- [2026-10-02 Windows](docs/verification/2026-10-02-windows.md): all four providers;
  the record does not identify each session's surface as a tab or console window.
- [2026-10-01 Windows](docs/verification/2026-10-01-windows.md): tab and console-window surface
  checks, including focus and close.
- [2026-09-29 macOS](docs/verification/0.0.9.md): Pi in iTerm2 completed initial and follow-up
  requests and close, with fixture-derived consent and an explicit model.

## Install

Download the archive for your platform and its matching `.sha256` file from
[GitHub Releases](https://github.com/jy1655/tabcli/releases). Prebuilt archives need no Rust:
`tabcli-<version>-aarch64-apple-darwin.tar.gz` for Apple Silicon macOS, or
`tabcli-<version>-x86_64-pc-windows-msvc.zip` for x64 Windows.

For version 0.2.5, verify the checksum in the download directory before extracting.
On macOS, continue only if the checksum command reports `OK`:

```sh
shasum -a 256 -c tabcli-0.2.5-aarch64-apple-darwin.tar.gz.sha256
tar -xzf tabcli-0.2.5-aarch64-apple-darwin.tar.gz
```

On Windows, run these commands in PowerShell:

```powershell
$archive = "tabcli-0.2.5-x86_64-pc-windows-msvc.zip"
$expected = (Get-Content "$archive.sha256").Split()[0]
$actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch" }
Expand-Archive $archive -DestinationPath .\tabcli
```

If verification fails, stop and download the archive and checksum again from the same release.
Put the extracted `tabcli` (macOS) or `tabcli.exe` (Windows) in a directory on your `PATH`.

To install from source, use Rust 1.97.1 or newer and Cargo:

```sh
cargo install --git https://github.com/jy1655/tabcli --tag v0.2.5 --locked
```

The package is not on crates.io. The crates named `agent-bridge` and `tab-cli` there are unrelated
projects. The executable and Cargo package are both named `tabcli`.

Before the first launch, install and sign in to the provider CLI, and resolve its workspace-trust
prompt for your project. The iTerm2, Terminal.app, and Ghostty integrations use macOS Automation.
If launch reports an Apple Events permission error, follow the
[macOS permission checks](docs/macos-permissions.md). Native Windows requires PowerShell 7
(`pwsh.exe`) on `PATH`, even when you invoke Bridge from cmd.

## Everyday use

Return without waiting for a result with
`tabcli ask codex --prompt "Review this project." --detach`.
Then wait using the session and request ids it prints:

```sh
tabcli result session-K7m2Qx --request request-1791285000000000000-4217-0 --wait
```

Find recorded sessions with `tabcli sessions --sort updated`; this command also repairs interrupted
lifecycle changes and dead owners, so it is not read-only.

Read records without changing them with `tabcli inspect session-K7m2Qx --timeline` after a timeout
or delivery error.

Check your installed setup with `tabcli self-test codex --workspace .`; it makes real model calls,
opens a terminal surface, checks an initial result and follow-up, and closes only its own session.
It uses your ordinary state root by default. If it fails, read the failed step and cleanup outcome
before running it again; it does not approve new workspace trust.

See the [CLI reference](docs/cli.md) for options, output fields, and recovery steps.

## Before you rely on it

Bridge is not a model client or a sandbox. It cannot attach to a session it did not launch.
It runs no hosted service of its own; the provider CLIs make requests to their configured services
under your existing login.

**A Bridge-launched Claude session accepts cross-session messages from other Claude sessions on
the same account. With `--yolo`, that work runs without Claude's permission checks.** Do not use
that combination unless you control the account's other Claude sessions. Read
[Claude inbound messages](docs/security-and-data.md#claude-inbound-messages).

`--yolo` forwards the provider's own bypass flag; Bridge adds no sandbox. Pi is different: its
`--approve` grants project approval while its native tool policy stays in effect. Existing provider
settings still apply. See [permission modes](docs/security-and-data.md#permission-modes).

Workspace trust stays with the provider. Bridge can reuse verified existing consent for an exact
workspace; it does not grant new trust. If a trust dialog holds up work, review it in the managed
surface. See [workspace trust and consent](docs/security-and-data.md#workspace-trust-and-consent).

A delivery whose outcome is uncertain is never resent. Inspect the original request and wait for
it, or explicitly close the session; repeating the prompt risks duplicate work. See
[delivery and trust boundaries](docs/security-and-data.md#workspace-trust-and-consent).

Session records contain results and retained prompt text, stored unencrypted under
`~/.agent-bridge/native-sessions`. Closing a session retains its records; some temporary delivery
files are removed earlier. Windows uses `HOME`, then `USERPROFILE`, for the home directory;
`AGENT_BRIDGE_NATIVE_STATE_DIR` overrides the root. Review records before sharing them; see
[local records](docs/security-and-data.md#local-records).

To uninstall, close your sessions, follow
[Close sessions and remove Bridge data](docs/security-and-data.md#list-close-and-remove-sessions),
then remove the downloaded executable or run `cargo uninstall tabcli` for a Cargo installation.
Provider history and credentials are separate.

## Upgrading from `agent-bridge` 0.1.x

The executable is now `tabcli`. Your sessions, settings, and consent records are read as they are;
the state directory and `AGENT_BRIDGE_*` environment variables keep their names. No `agent-bridge`
alias is installed, so update your scripts.

Keep the old executable where it is until the sessions it launched are closed: their hooks call
it by path. Installing `tabcli` does not update those running sessions' hooks.

## Documentation

- [CLI reference](docs/cli.md): commands, output, and recovery steps.
- [Terminals](docs/terminals.md): surface selection, settings, ownership, and verification limits.
- [Providers](docs/providers.md): CLI requirements, delivery paths, and result handling.
- [Security and data](docs/security-and-data.md): permissions, trust, and local records.
- [macOS permissions](docs/macos-permissions.md): Automation and provider approvals.
- [Architecture](docs/architecture.md): modules, session records, and lifecycle contracts.
- [Testing](docs/testing.md): automated checks and manual authenticated runs.
- [Release notes](docs/releases/README.md): changes and recorded verification by version.

## Contributing and security

Read [Contributing](CONTRIBUTING.md) to report a bug or propose a change in English or Korean.
Report vulnerabilities privately through the [security policy](SECURITY.md), not a public issue.

## License

TAB is licensed under the [MIT License](LICENSE); see [third-party notices](THIRD_PARTY_NOTICES.md).
It is an independent project, not affiliated with or endorsed by OpenAI, Anthropic, Google, or the
makers of the terminals and CLIs it drives.
