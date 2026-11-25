# Current Herdr Surface Boundary

- Herdr is a Linux, macOS and Windows surface (`--terminal herdr`, also `hc` and `herdr-core`) in
  an already running Herdr Core. TAB speaks newline JSON-RPC 2.0, protocol generation 1, on
  the core's own socket or, on Windows, its named pipe (measured with hc 0.1.21 on macOS and
  Linux, 2026-11-18; on Windows from the hc 0.1.21 source and one Pi run below). It never runs `hc`, whose
  CLI starts a missing core, and never starts, restarts or stops a core. Other platforms refuse
  the kind. The Windows-only rules are at the end of this file.
- When the core is absent, direct the user to open Herdr from the desktop or start hc outside
  an agent session. Starting it from an agent can contaminate every terminal with that agent's
  caller markers. Removing markers in TAB's launch host protects its own children, not
  unrelated terminals or the core; never repair the core by restarting it as a side effect.
- The socket is the one `hc` uses (herdr-core `paths.rs`): `HC_SOCKET`, else `socket` in
  `$XDG_CONFIG_HOME/herdr-core/config.toml`, else `<session>.sock` in
  `$XDG_RUNTIME_DIR/herdr-core`, `/run/user/<uid>/herdr-core` or the state directory of
  `HC_SESSION`. An unreadable or unparsable config, an invalid session name, and a relative or
  too long path are refused, never guessed around. The socket must be this user's, in a
  directory others cannot write, served by a process of this user.
- Every action opens one connection, checks `hello` (generation 1, the six methods TAB
  uses and, for an existing handle, the recorded permanent `core.id`) and acts on that same
  connection. A missing or refusing socket or a different core id is unknown, never absence;
  only `not_found` from the recorded core proves a terminal gone (addresses are not reused on
  a core). Calls pass no `caller`, so TAB marks nothing seen for another terminal.
- `terminal.run` gets argv `[tabcli, native-herdr-host, <session directory>,
  --launch-command, <script>]`, `no_notify`, `keep: false`, `rm: false`, the
  labels `tabcli-session` and a random `tabcli-launch`, and a random local name
  (`tabcli-` and 24 hexadecimal digits). On macOS and Linux, on the launch connection, verify the local caller's
  marker, live terminal and program ancestry before setting `parent` and `outlive_parent`.
  With no verified caller, and on Windows even inside Herdr, use `no_parent`. A parented terminal appears under its caller in
  the app and counts in hc child waits. hc stores `die_with_parent: false` and `notify: false`
  in its terminal registry. The created terminal
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
  or fails. TAB never runs it again: it reads the launch's name on the same core (a name
  resolves locally, to the live terminal holding it or else the latest finished one) and
  removes that terminal only when its address, argv, label and name are all the launch's.
  `terminal.list` is not used, because hc's list also asks every configured remote machine.
  TAB retains the handle when the identity is complete but removal failed, and otherwise
  reports the name and that nothing starts without a binding.
  If saving the binding for a confirmed terminal fails and removal also fails, the adapter
  returns that same confirmed handle to the launcher. The launcher uses the current Claim
  and Close records to decide whether to save it. A later close checks its identity again.
- The core gives programs its own environment, not the caller's, and it can carry another
  session's markers: the core on the owner's Mac on 2026-11-18 carried Claude Code's
  child-session and messaging variables. Every launch host removes the adapters' caller markers
  before starting the provider. The macOS host also removes them around shell capture. Do not ask users to restart their
  core for this.
- `terminal/herdr/identity.rs` owns the operation requirements for a bound handle. It opens the
  recorded core and checks the local address, `created_ms`, host invocation and directory in
  argv, recorded program PID and `keep: false`. For a live terminal, a readable birth must
  match. Send, screen and verification also require a live terminal and an observable program,
  and match the last directory component to the managed Session id. Presence and ordinary
  close use the exact directory path and accept either terminal state.
  A terminal set to keep (`hc keep`) can run a replacement program and
  is refused until keep is off. For a temporary pane, the user can adopt the terminal with
  `herdr-app do terminal.keep` and release it with `terminal.park`, which disables core keep.
  TAB never adds a space label or changes keep to counter the app. If the pane ends
  with keep still set, hc 0.1.21 only lets a user change keep on a live terminal. Document
  manual removal of the verified finished address on the recorded core before retrying
  explicit close. A remote address (`machine/t7`) is never accepted.
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
  reads (a finished terminal is only removed). On macOS and Linux, when the owner has already exited without a
  close intent, explicit close verifies its bound identity and reads the exact finished,
  unkept terminal again. It checks the launch label, sends one `terminal.rm`, and confirms
  absence on that core. It never sends `terminal.kill` on this path. Handles created by
  0.4.45 and 0.4.46 omit the nonce; for those, the original identity checks also require the
  launch label to agree with the terminal's generated name. A failed read or removal keeps
  the handle for retry. Cleanup after binding fails uses the ordinary close proof. Cleanup
  before confirmation checks the full launch argv, label and name, plus the creation
  time and PID from its first read at that address. It refuses a terminal set to keep. A birth
  is needed to retain a handle for retry. The core has no conditional kill or remove, so another
  client of the same user can still change the terminal between TAB's last read and its action.
  hc does not sweep detached processes. TAB touches no other terminal and calls no core operation.
- An explicit `--terminal` wins. Otherwise `HC_TERMINAL_ID` with `TERM_PROGRAM=ghostty` (the
  identity hc gives every program) selects Herdr, on Windows only for a provider whose adapter
  runs there;
  the marker under another `TERM_PROGRAM` was inherited and is ignored. Selection reads only
  the environment and, on Windows, the requested provider. Launch admission
  (`terminal::admit_selection`) then requires the caller's terminal to be live on the
  addressed core with this process among its program's descendants, and refuses otherwise
  without opening another terminal. An explicit `--terminal herdr` can launch outside Herdr.
  On macOS and Linux both selection modes check the caller on the connection that creates the child before
  naming a parent.
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
  `C:\` for a missing profile, TAB refuses rather than read a configuration from there.
- hc serves the pipe to SYSTEM and its own user only, rejects remote clients and labels it
  with its integrity level; its own client checks no server. TAB connects as hc does
  (`SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`, so a server cannot act as TAB's user),
  then requires the client end of a byte pipe whose server process (`GetNamedPipeServerProcessId`)
  has this user's SID in its token, the Windows counterpart of `SO_PEERCRED`. A missing pipe is
  a missing core; a pipe refusing access is an elevated or foreign core and is refused with that
  hint. The process is opened for limited query in the Windows process module; the security
  module only reads the token of the handle it is lent, and opens no process (its dependency
  policy, `tests/windows_security_policy.rs`). Both transports share one framing (`Framed`): a
  request is unsent only while none of its
  bytes was written. Pipe reads and writes are overlapped and waited for until the deadline; a
  pending one is then cancelled and its completion collected before its buffer is released, and
  bytes it moved before the cancellation count as written.
- A Windows program birth is its creation time (`host_creation_time`, from `GetProcessTimes`);
  an exited process, held or not, and a free PID have none. Caller ancestry reads parents from
  a process snapshot and accepts a parent only while it is live and created no later than its
  child, because Windows keeps a dead parent's PID, which a later process can hold. A parent
  that cannot be inspected ends the walk, which can then only refuse.
- The Windows launch host cannot exec. Its session directory is admitted as on Linux and then
  read again from a handle opened on the directory itself (`FILE_FLAG_OPEN_REPARSE_POINT`): no
  reparse point, and the trust store's access-list rule, where `FILE_DELETE_CHILD` and `DELETE`
  also count as changing a directory (`consent::windows_directory_is_private`; the store's own
  rule is unchanged). After the same binding wait, launch judgment and locked
  re-check as the Linux host (`bound_host::prepare_bound_launch_with`), it flushes `CONIN$` (a
  console it cannot flush refuses the launch), keeps itself running through Ctrl+C and
  Ctrl+Break with a handler routine (not inherited, unlike ignoring them), and starts PowerShell
  7 from the core's `PATH` with the managed console root's exact arguments
  (`console_root_arguments`, which also removes `TERM`), through the helper command that drops
  caller markers. It exits with the root's code. The root shares the host's console and the
  core's job object, so `terminal.kill` (hc ends the job) ends the whole session.
- Each provider adapter answers whether it runs on a surface (`surface_support`, required, no
  default), before anything is created (`provider::admit_surface`). On Windows Herdr, Codex
  refuses: its initial prompt is the injected console paste (#124), text without Return and one
  Return against the exact accepted screen, which `terminal.send` cannot do, and the
  launch-argument route used elsewhere would put the multi-line prompt on a Windows command
  line (32,767 characters; 8,191 and no line break through `codex.cmd`). Claude (official
  messaging; its dialog is left to the user, as on macOS and Linux), Agy (paste through the
  surface behind its log readiness and receipt) and Pi run there. Inside an hc terminal the
  automatic choice is Herdr when the provider's adapter runs there and the managed console
  otherwise; a reopen, whose provider is unknown at selection, keeps the console.
- A Herdr handle on Windows never reaches the console control helper: every surface operation
  resolves its adapter through the one Windows lookup (`windows/dispatch.rs`) after the kind's
  declared refusals, so guarded dialog input is refused as on the other platforms. Pi and
  Agy keep their Windows framing, readiness and receipts; only the transport is `terminal.send`.
  Agy's Windows readiness also needs `Full redraw completed`, measured on the console only;
  without it the paste is withheld and reported not sent.
- The Windows ownership proof checks the Herdr terminal on its core, then the owner's process
  identity. The owner's death proves nothing about the terminal: a dead owner's repair, in any
  repairable state, closes the exact Herdr handle through the verified core under the close
  transaction's claim. It may end a live, identity-verified launch host. A close that fails (no core, another core, a changed terminal)
  restores the handle and leaves state and claim for the next close. Only the core's
  `not_found` proves the terminal gone. A console's ended session still settles from its records.
- The PowerShell `attach_command` sets `HC_SOCKET` in its own script block and restores the
  earlier value (or its absence) in `finally`, so later commands keep their own core.
- Windows tests: endpoint resolution, the shared framing, the adapters' surface answers and the
  Windows lookup (its routing and declared refusals, over a recording console adapter and the
  Herdr adapter on a scripted core) run on every platform; the pipe transport (a real pipe of
  this user), process births, parents, tokens, the installed lookup against an absent pipe,
  the directory admission, the dead-owner repair, the attach line under PowerShell 7 and the
  binding wait run on the Windows CI runner. Live (reported by the
  verification session, 2026-11-18): the Windows PR head (505f0e7) passed the five native
  Windows self-test steps with Pi 1.5.0 on hc 0.1.21, Windows 11 x64, as a standard desktop
  user, with distinct turns and no terminal left after close; the release before it refused.
  Claude and Agy on Windows Herdr have no live run.
