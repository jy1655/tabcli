# Current Herdr Surface Boundary

- Herdr is a macOS surface (`--terminal herdr`, also `hc` and `herdr-core`) in an already
  running Herdr Core. Bridge speaks newline JSON-RPC 2.0, protocol generation 1, on the core's
  own socket (measured with hc 0.1.21, 2026-11-18). It never runs `hc`, whose CLI starts a
  missing core, and never starts, restarts or stops a core. Other platforms refuse the kind.
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
  it finishes (measured 2026-11-18). The handle records address, socket, core id,
  `created_ms` and that program incarnation.
- The program is the macOS launch host (Current macOS Shell Startup Boundary). It binds only
  when `HC_TERMINAL_ID` names the bound address and its own PID is the recorded program, and
  refuses at the launch receipt deadline otherwise. That is the fence for a `terminal.run`
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
- The core gives programs its own environment, not the caller's, and it can carry another
  session's markers: the core on the owner's Mac on 2026-11-18 carried Claude Code's
  child-session and messaging variables. The launch host's capture, then removal of the
  adapters' caller markers, keeps them from the provider. Do not ask users to restart their
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
  identity hc gives every program) selects Herdr; the marker under another `TERM_PROGRAM` was
  inherited and is ignored. Selection reads only the environment. Launch admission
  (`terminal::admit_selection`) then requires the caller's terminal to be live on the
  addressed core with this process among its program's descendants, and refuses otherwise
  without opening another terminal. An explicit `--terminal herdr` does not check the caller.
- OSC 7501 status words, `hc wait` and templates are never request, completion or close
  evidence. Provider launch options under Herdr belong to that provider's boundary.
- Unit tests use a scripted core plus a real-socket connection test. Live launch, input,
  screen and close with each provider are recorded separately; do not cite these tests as
  live verification.
