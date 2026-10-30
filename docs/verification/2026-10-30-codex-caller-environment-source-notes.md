# Codex caller environment source notes

## Scope

These notes record why every Bridge launch removes five Codex caller markers, and what has
not been verified. They are source evidence and names-only observations, not runtime
verification of the change.

- Codex source: tag `rust-v0.166.0`, commit `6ff1e1ca3b30fc8b914720681fcf5fe62e95f856`, read
  as data and never built or run. Paths below are relative to `codex-rs/` unless they start
  with `codex-cli/`. The installed CLI on the investigating Mac reported `codex-cli 0.166.0`.
- Bridge change: `CODEX_CALLER_SESSION_MARKERS` in `src/native/provider/codex/mod.rs`. Codex
  applies it to launches, probes and `codex queue`; Claude, Agy and Pi select it by reference;
  the union of adapter lists carries it to terminal and helper processes.

## The five names

| Name | Writer | Reader and effect |
| --- | --- | --- |
| `CODEX_SESSION_ID` | `core/src/exec_env.rs:41-42` inserts the root-session id; unified exec calls it at `core/src/unified_exec/process_manager.rs:1504`. | No production code reads it to select Codex's own session. Every inheriting child of the caller sees the caller's root session. |
| `CODEX_THREAD_ID` | Unified exec removes case variants on Windows and inserts the current thread (`core/src/unified_exec/process_manager.rs:1497-1502`); shell policy adds a supplied thread (`protocol/src/shell_environment.rs:165-168`). | No Codex reader selects a thread from it; `codex queue` takes a required `--thread` (`cli/src/queue_cmd.rs:21-23`). Inheriting children see the caller's thread. |
| `CODEX_TOOL_CALL_ID` | `protocol/src/shell_environment.rs:30-41` replaces it (case-insensitively on Windows); unified exec supplies the call id at `core/src/unified_exec/process_manager.rs:1503`. | No Codex reader selects a turn or session from it. Inheriting children see the caller's tool call. |
| `CODEX_DAEMON_SHUTDOWN_SOCKET` | The Windows PID backend sets it to `1` on its own app-server child (`app-server-daemon/src/backend/pid_start.rs:191-194`). | On Windows, a control-socket app-server grants managed shutdown access when the name is present, whatever its value (`app-server/src/lib.rs:830-844`). The `/daemon/shutdown` route is otherwise refused with 403 (`app-server-transport/src/transport/unix_socket.rs:197-202`) and still requires the receiving process's own PID as its first message (`unix_socket.rs:251-267`). Inherited, it enables that route on an unrelated local-socket server; it does not by itself stop anything. Off Windows the check is disabled. |
| `CODEX_DAEMON_SHUTDOWN_FILE` | The Windows PID backend sets its updater's shutdown-file path (`app-server-daemon/src/backend/pid_start.rs:195-207`). | The Windows updater polls the path every 50 ms and consumes a request only when the file holds exactly its own decimal PID (`app-server-transport/src/daemon_shutdown.rs:18-49`); an unreadable path ends the updater's wait (`app-server-daemon/src/update_loop.rs:799-807`). Inherited, it names another process's control file. |

Codex does not clean these names out of its own process environment:

- Command hooks capture the process environment once (`hooks/src/registry.rs:83`) and replay
  it into every hook command, filtering only restricted names
  (`hooks/src/engine/command_runner.rs:484-495`; restricted list at
  `protocol/src/shell_environment.rs:14-28`). No current session, thread or tool-call id is
  inserted.
- Legacy `notify` receives the same captured environment (`hooks/src/registry.rs:182`;
  `hooks/src/legacy_notify.rs:45-56`) and replays it after `env_clear`
  (`hooks/src/registry.rs:407-421`).
- The `.env` loader refuses `CODEX_` names from that file but does not remove existing
  process variables (`arg0/src/lib.rs:270-303`).

A Codex started from a Codex tool shell therefore runs its hooks, notify and other
inheriting descendants with the caller's ids, even though its own tool shells receive
fresh ones.

## Deliberate exclusions

The list names five exact spellings; Bridge never strips a `CODEX_` prefix. These stay:

- Configuration and credentials: `CODEX_HOME` (`utils/home-dir/src/lib.rs:14-17`),
  `CODEX_SQLITE_HOME`, `CODEX_CA_CERTIFICATE`, and authentication or endpoint settings such as
  `CODEX_API_KEY`.
- Install hints written by the JS launcher (`codex-cli/bin/codex.js:217-233`) and read for the
  installation method (`install-context/src/lib.rs:123-130`): `CODEX_MANAGED_BY_*`,
  `CODEX_MANAGED_PACKAGE_ROOT`.
- Informational and compatibility names: `CODEX_CI` (`core/src/unified_exec/process_manager.rs:103`),
  `CODEX_VERSION` (`core/src/exec_env.rs:43-49`), `CODEX_PERMISSION_PROFILE`
  (`core/src/exec_env.rs:56-71`), `CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS`
  (`core/src/exec_env.rs:76-82`).
- Sandbox and proxy context: `CODEX_SANDBOX` (`core/src/sandboxing/mod.rs:180`, read at
  `login/src/auth/default_client.rs:477`), `CODEX_SANDBOX_NETWORK_DISABLED`
  (`core/src/spawn.rs:87`), and the network proxy names.

Removing the five names does not make the environment clean: stale informational or install
hints can remain, and an explicit setting can reintroduce any name.

## Other providers

Bounded local evidence, not a behavioral claim:

- The Claude Code Codex plugin 1.0.6 starts `codex app-server` with `options.env ?? process.env`
  (`scripts/lib/app-server.mjs`), so a Claude descendant can forward the names.
- Pi 1.4.0 bundles an agent-detection table that reads `CODEX_THREAD_ID`
  (`dist/bundle/chunks/jiti-loader-GL4HJFAD.js`). This does not show that Pi uses it as
  session identity.
- The installed Agy binary contains none of the five literals; that cannot exclude a
  dynamically built name or a descendant.

## Observations

- 2026-10-29, native Windows, installed `tabcli 0.4.30`, Claude Code 2.1.312, issue #111: with
  `tabcli` started from a Codex tool shell (Codex shell tool, PowerShell, uv, Python), the new
  Windows Terminal, the `native-console-host` tab host, the PowerShell root and the managed
  Claude process all carried the five names. Only names were recorded, not values.
- 2026-10-30, this Mac, a Codex 0.166.0 tool shell: a names-only listing of `CODEX_` variables
  showed `CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS`, `CODEX_CI`, `CODEX_MANAGED_BY_NPM`,
  `CODEX_MANAGED_PACKAGE_ROOT`, `CODEX_SESSION_ID`, `CODEX_THREAD_ID`, `CODEX_TOOL_CALL_ID` and
  `CODEX_VERSION`. Neither shutdown name was present. No values were read.

## Verification of the change

Deterministic tests pin the five names with fixed spellings in every adapter's probe list and
launch plan (both platform branches), the Codex queue command, the Claude messenger and reopen
plan, the helper union, the Unix wrapper integration (synthetic values must not reach a fake
provider; a synthetic `CODEX_HOME` must), the doctor's version probe, and the Windows
environment block in upper, lower and mixed case. The Windows tests run only on the Windows CI
runner.

## NOT VERIFIED

- Native Windows runtime after this change, including the cold Windows Terminal start and a
  later user-opened tab (#111).
- Live values of the names inside Codex hooks and notify.
- An actual shutdown request through either Windows marker.
- Any behavioral effect of the names on Claude, Agy or Pi.

Revisit an entry when Codex stops exporting it, or isolates it across every child path,
including hooks, notify and nested launches.
