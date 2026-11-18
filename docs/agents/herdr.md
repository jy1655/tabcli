# Current Herdr Surface Boundary

- Herdr is a Linux, macOS and Windows surface (`--terminal herdr`, also `hc` and `herdr-core`) in
  an already running Herdr Core. Bridge speaks newline JSON-RPC 2.0, protocol generation 1, on
  the core's own socket or, on Windows, its named pipe (measured with hc 0.1.21 on macOS and
  Linux, 2026-11-18; Windows from the hc 0.1.21 source, not yet live). It never runs `hc`, whose
  CLI starts a missing core, and never starts, restarts or stops a core. Other platforms refuse
  the kind. The Windows-only rules are at the end of this file.
- When the core is absent, direct the user to open Herdr from the desktop or start hc outside
  an agent session. Starting it from an agent can contaminate every terminal with that agent's
  caller markers. Removing markers in Bridge's launch host protects its own children, not
  unrelated terminals or the core; never repair the core by restarting it as a side effect.
- The socket is the one `hc` uses (herdr-core `paths.rs`): `HC_SOCKET`, else `socket` in
  `$XDG_CONFIG_HOME/herdr-core/config.toml`, else `<session>.sock` in
  `$XDG_RUNTIME_DIR/herdr-core`, `/run/user/<uid>/herdr-core` or the state directory of
  `HC_SESSION`. An unreadable or unparsable config, an invalid session name, and a relative or
  too long path are refused, never guessed around. The socket must be this user's, in a
  directory others cannot write, served by a process of this user.
- Every action opens one connection, checks `hello` (generation 1, the six methods Bridge
  uses and, for an existing handle, the recorded permanent `core.id`) and acts on that same
  connection. A missing or refusing socket or a different core id is unknown, never absence;
  only `not_found` from the recorded core proves a terminal gone (addresses are not reused on
  a core). Calls pass no `caller`, so Bridge marks nothing seen for another terminal.
- `terminal.run` gets argv `[tabcli, native-herdr-host, <session directory>,
  --launch-command, <script>]`, `no_parent`, `no_notify`, `keep: false`, `rm: false`, the
  labels `tabcli-session` and a random `tabcli-launch`, and a random local name
  (`tabcli-` and 24 hexadecimal digits). Without `no_parent` a terminal started inside Herdr
  ends with the caller's terminal, and hc types notes into the caller. The created terminal
  is read back on the same connection (argv, launch label, name, live, not kept) with its
  program's PID and birth: `Terminal.pid` is the program's own PID and stays after
  it finishes (measured 2026-11-18). Linux verifies the socket peer with `SO_PEERCRED` and
  records program birth as boot ID plus procfs start ticks (Windows: below). The handle
  records address, socket, core id, `created_ms` and that program incarnation.
- The Linux host runs the [bound bash handoff](linux.md). The macOS host runs the
  [shell environment capture](macos-shell.md). The Windows host is below. Each host binds
  only when `HC_TERMINAL_ID` names the bound address and its own PID is the recorded program,
  and refuses at the launch receipt deadline otherwise. That is the fence for a `terminal.run`
  whose result is uncertain: no reply, a reply without a local address, or an error other
  than those herdr-core gives before the terminal exists (`invalid_params`, `name_taken`,
  `spawn_failed` and the other kinds in the adapter). herdr-core 0.1.21 can answer
  `unavailable` or `internal` after creating the terminal, when saving its registry times out
  or fails. Bridge never runs it again: it reads the launch's name on the same core (a name
  resolves locally, to the live terminal holding it or else the latest finished one) and
  removes that terminal only when its address, argv, label and name are all the launch's.
  `terminal.list` is not used, because hc's list also asks every configured remote machine.
  Bridge retains the handle when the identity is complete but removal failed, and otherwise
  reports the name and that nothing starts without a binding.
  If saving the binding for a confirmed terminal fails and removal also fails, the adapter
  returns that same confirmed handle to the launcher. The launcher uses the current Claim
  and Close records to decide whether to save it. A later close checks its identity again.
- The core gives programs its own environment, not the caller's, and it can carry another
  session's markers: the core on the owner's Mac on 2026-11-18 carried Claude Code's
  child-session and messaging variables. Every launch host removes the adapters' caller markers
  before starting the provider. The macOS host also removes them around shell capture. Do not ask users to restart their
  core for this.
- Every later operation requires the recorded address, core id and `created_ms`, an argv
  naming this session's directory, the recorded program PID, `keep` false and, while live,
  the recorded birth. A terminal set to keep (`hc keep`) can run a replacement program and
  is refused until keep is off. A remote address (`machine/t7`) is never accepted.
- Input is `terminal.send` with the exact prompt text, without `force` (which answers a
  blocked dialog) and without `caller`. hc brackets the paste itself when the program asked
  for it, so the envelope is raw. A refusal the core gives before typing (`blocked`,
  `status_mismatch`, `not_found`, `finished` and the other kinds in the adapter) is not sent;
  `unavailable`, `cancelled`, an unknown kind, or a written request without a reply is
  uncertain and never replayed. hc can hold a send for its `draft_wait` while a person types;
  a deadline that ends first is uncertain. A prompt above 1 MiB is refused before sending.
- Screen reads use `terminal.read` with `lines: 0` only: a history read can scroll a
  full-screen program. Guarded dialog answers are refused (no call compares the screen and
  presses a key at once); the user answers a trust dialog in Herdr or `hc attach`.
- Close runs under the existing owned-close authority: `terminal.kill` at most once per
  close, then `terminal.rm` of the finished terminal. Every read before a kill or removal is
  checked against the recorded creation and program again, so a terminal set to keep or
  running a replacement in between is left alone and its handle retained. A close whose kill
  reply was lost fails instead of repeating it, and the next close finishes from what it
  reads (a finished terminal is only removed). The same rule applies when a launch removes a
  terminal it created but could not use. The core has no conditional kill or remove, so
  another client of the same user can still change the terminal between Bridge's last read
  and its action; Bridge does not claim more. hc does not sweep detached processes. Bridge
  touches no other terminal and calls no core operation.
- An explicit `--terminal` wins. Otherwise `HC_TERMINAL_ID` with `TERM_PROGRAM=ghostty` (the
  identity hc gives every program) selects Herdr, on Windows only for a provider it carries;
  the marker under another `TERM_PROGRAM` was inherited and is ignored. Selection reads only
  the environment and, on Windows, the requested provider. Launch admission
  (`terminal::admit_selection`) then requires the caller's terminal to be live on the
  addressed core with this process among its program's descendants, and refuses otherwise
  without opening another terminal. An explicit `--terminal herdr` does not check the caller.
- OSC 7501 status words, `hc wait` and templates are never request, completion or close
  evidence. Provider launch options under Herdr belong to that provider's boundary.
- Unit tests use a scripted core plus a real-socket connection test. Live launch, input,
  screen and close with each provider are recorded separately; do not cite these tests as
  live verification.
- Windows endpoint (herdr-core 0.1.21 `paths.rs`, `platform/windows.rs`): `HC_SOCKET`, else
  `socket` in `config.toml` under `XDG_CONFIG_HOME`, `APPDATA` or `USERPROFILE\AppData\Roaming`,
  else `\\.\pipe\herdr-core-<user>-<session>.sock`, where `<user>` keeps only the ASCII letters
  and digits of `USERNAME`. Only a local pipe (`\\.\pipe\<name>`, at most 256 characters) is
  accepted: another machine's pipe, a file or a socket path is refused. Where hc falls back to
  `C:\` for a missing profile, Bridge refuses rather than read a configuration from there.
- hc serves the pipe to SYSTEM and its own user only, rejects remote clients and labels it
  with its integrity level; its own client checks no server. Bridge connects as hc does
  (`SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`, so a server cannot act as Bridge's user),
  then requires the client end of a byte pipe whose server process (`GetNamedPipeServerProcessId`)
  has this user's SID in its token, the Windows counterpart of `SO_PEERCRED`. A missing pipe is
  a missing core; a pipe refusing access is an elevated or foreign core and is refused with that
  hint. Both transports share one framing (`Framed`): a request is unsent only while none of its
  bytes was written. Pipe reads and writes are overlapped and waited for until the deadline; a
  pending one is then cancelled and its completion collected before its buffer is released, and
  bytes it moved before the cancellation count as written.
- A Windows program birth is its creation time (`host_creation_time`, from `GetProcessTimes`);
  an exited process, held or not, and a free PID have none. Caller ancestry reads parents from
  a process snapshot and accepts a parent only while it is live and created no later than its
  child, because Windows keeps a dead parent's PID, which a later process can hold. A parent
  that cannot be inspected ends the walk, which can then only refuse.
- The Windows launch host cannot exec. After the same binding wait, launch judgment and locked
  re-check as the Linux host (`bound_host::prepare_bound_launch_with`), it flushes `CONIN$` (a
  console it cannot flush refuses the launch), keeps itself running through Ctrl+C and
  Ctrl+Break with a handler routine (not inherited, unlike ignoring them), and starts PowerShell
  7 from the core's `PATH` with the managed console root's exact arguments
  (`console_root_arguments`, which also removes `TERM`), through the helper command that drops
  caller markers. It exits with the root's code. The root shares the host's console and the
  core's job object, so `terminal.kill` (hc ends the job) ends the whole session.
- A Windows Herdr terminal carries Pi only (`terminal::admit_provider`, before anything is
  created): Codex's Windows initial prompt is the injected console paste (#124), Claude's
  Windows workspace approval answers a console dialog and its Windows owner checks were verified
  in the console only, and Agy's Windows receipts and readiness were measured on the console
  only. Inside an hc terminal the automatic choice is Herdr for Pi and the managed console for
  the others, as before; a reopen, whose provider is unknown at selection, keeps the console.
  Lift a refusal only with that provider's Windows Herdr live evidence.
- Pi keeps its Windows input contract on Herdr (single-line framing, startup readiness and the
  per-turn input receipt; see the Pi boundary); only the transport is `terminal.send`. A Herdr
  handle on Windows never reaches the console control helper: every surface operation is
  dispatched by kind, and guarded dialog input is refused as on the other platforms.
- The Windows ownership proof checks the Herdr terminal on its core, then the owner's process
  identity. A dead owner's repair closes a running session's terminal through this adapter's
  close. An Exited or Failed session is marked closed without a core call, as for the console,
  so a terminal whose provider ended on its own stays on the core, finished, until `hc rm`.
- Windows tests: endpoint resolution and the shared framing run on every platform; the pipe
  transport (a real pipe of this user), process births, parents, tokens, the dispatch by kind
  and the binding wait run on the Windows CI runner. A Windows Herdr launch, input, screen and
  close with Pi still need a live run; do not cite these tests as one.
