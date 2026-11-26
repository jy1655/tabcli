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
- On Linux and Windows, `bound_host` logs a startup-input flush or launch-shell start
  failure before attempting to record it. Publication requires the host's live birth to
  match the birth in its proven binding and rechecks the pending Claim and exact Surface
  under the lifecycle lock. Launch wait and repair read it through the same settlement as
  the macOS host. A missing or different birth keeps the refusal in the launch log. A
  PowerShell wait error after successful spawn also reaches only the launch log.
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
  reads (a finished terminal is only removed). A close sends at most one `terminal.rm` with one
  confirming read; `not_found` from the recorded core answers it. What holds around those two
  calls, and how it is enforced:
  - Only `authority::grant` (`terminal/herdr/authority.rs`) mints a `Kill` or `Remove` grant,
    and only from an observation that the verified connection itself produced (`Seen`, whose
    fields are private to `terminal/herdr/linked.rs` and which only `Linked::observe` makes).
    That observation carries the connection's core id and the terminal's address together.
    `grant` takes no core id or address from its caller and refuses an observation from a core,
    or of an address, other than the one its basis records (tests with two cores holding `t7`,
    and with `t8` beside `t7` on one core).
  - In production builds the installed code sends a request to a core only through `Linked`,
    by visibility: `Connection::call` takes a `Permit` whose constructor is private to
    `linked.rs`, and the framed stream is private to `framed.rs`. `Linked` sends the two
    mutations only by consuming a grant, on the connection to the core the grant names, and
    `Linked::call` refuses both names.
  - A source scan (`only_the_verified_connection_names_the_mutation_calls`) is a tripwire: it
    fails when a production source other than `linked.rs` spells either method name. It does
    not catch a name assembled from parts.
  - Not covered: test builds can reach the raw stream (`Framed::stream`, for the framing tests),
    and the type system does not stop a new `Connection` implementation from forwarding the
    `Permit` it receives to another request. The installed implementations do not.

  The evidence a close on the creation record carries (`CreationEvidence`) has private fields
  in `owner_exit`, its only constructor. Herdr's registration declares that its core retains a
  finished terminal, and one rule applies to it on every OS (the owner's decisions of
  2026-11-26, os-parity):
  - Repair's dead-owner decision never ends, removes or reads a gone owner's terminal: it keeps
    the handle for `close-session --explicit` and checks both handle paths again under the
    lifecycle lock before it settles a session, so a handle that a concurrent close restored
    is never consumed. The steps Repair takes before that decision still run: publishing a
    completed result, and settling a launch that was never confirmed.
  - `owner_exit::core_kept` chooses between the close with a live owner and the close on the
    creation record, for every owner record, and returns evidence, never an action. The owner
    record answers one question, from one typed read: is there an attested live incarnation of
    this session's own owner, from a record whose fields do not contradict each other (it
    carries no other platform's incarnation fields)? If so, the close with a live owner applies,
    unchanged. In every other case the close on the creation record applies: a record that
    cannot be read, none, a damaged, contradictory, unbound or another session's one (whose
    process is not inspected), a process that cannot be inspected, a PID-only record of a live
    process, a gone owner, a failed start, and on macOS and Linux an owner whose control read
    found its process gone after its birth read matched (`ControlAttestation::Exited`). A record
    contradicts itself when anything in it, nested identities included (a Terminal.app
    incarnation, a shell's birth), belongs to another platform; `record_conflicts` names every
    field. On the creation record nothing about the owner refuses a finished or absent
    terminal; a read or inspection failure only explains, beside each refusal's own recovery
    instructions, why no live owner was verified.
  - Only `close-session --explicit`, and the sessions it lists with `--with-started-from`, act on
    the creation record. On that path the cleanup after a refused reopen (`CloseCause::Cleanup`)
    is refused before the core is asked, a failed start included. With a verified live owner of
    this session, that cleanup still closes through the close with a live owner, as before.
  - The creation record proves the terminal and nothing else does: the recorded core, address,
    creation time, program PID, the exact session directory in the argv and the launch label,
    from a handle whose own record is whole before even `not_found` settles it. A record is
    whole when its decoded values pass three checks, each derived from the released writers
    (0.4.45 to 0.4.49): a host PID that the platform's process reader reports (1 to `i32::MAX`
    on macOS and Linux, any nonzero PID on Windows); a host birth of the platform's own kind
    with values its writers record (macOS microseconds below a second; a Linux boot id that is
    not all zero and any start tick, zero included; any Windows creation time); and a launch
    label of exactly 32 lowercase hexadecimal digits, which every generator wrote. Every
    Windows writer saved the label. The macOS and Linux writers of 0.4.45 and 0.4.46 omitted
    the field and never wrote `null`, so an explicit `null` is refused. A finished terminal is
    then removed and an absent one settled whatever the owner record says, including one that
    names a live process of another session or a recycled PID. On this path the state of the
    owner record never strands a session. The check stops there by decision: it does not
    examine the wire spelling (the shared decoder's aliases, unknown fields or case
    normalization), so a handle no released writer spelled that way can pass it. This path
    never ends a running terminal and removes only a terminal the verified core identifies, so
    settling an oddly spelled handle on `not_found` can at worst leave a terminal the core still
    lists, while refusing it would strand the session, which this rule exists to end.
  - A close on the creation record never ends a running terminal, whatever is known about the
    session's owner (the owner's decision of 2026-11-26, which replaced the kill on positive
    evidence of the owner's death). `grant` refuses: the terminal on the recorded core (its id)
    still runs and TAB could not verify a live owner for this session, with the read or
    inspection failure when there was one; nothing is sent and the handle is kept. The refusal
    names `hc get` and `hc kill` for the recorded endpoint in the platform's form, and asks for
    `tabcli doctor <session> --probe` and the session-directory check before a manual kill.
    The person waits for the terminal to finish or ends it there, and the next close removes
    it. The close with a live owner keeps its kill.
  - What changes from 0.4.49 (checked against the tag): there, on macOS and Linux, the ordinary
    close of a session whose owner had exited already refused a running Herdr terminal and
    removed only a finished one (`RemoveEnded`), and refused even that for a reused PID or an
    unbound, foreign or incomplete owner record. A running terminal was ended only through
    three shortcuts that reached `SurfaceOnly`: a failed start (status `failed`, launch never
    `spawned`, a bound owner gone), a `launching` or `failed` session without an owner record
    (both on Windows as well), and, on macOS and Linux, a close resumed from the close intent of
    an interrupted close whose owner had since died. On Windows, Repair (run by listing
    sessions, `tell` and an attached `ask` or `tell`) also closed a gone owner's terminal,
    running or not. All of those now leave a running terminal for the person, and when no live
    owner is verified a finished or absent one is removed or settled whatever the owner record
    says.
  - The closed record keeps the stored diagnosis, read under the lifecycle lock, when this close
    acts on the creation record. Without one, an owner record bound to this session whose
    process the OS reports gone records `native session process <pid> is no longer running`, as
    0.4.49's Repair wrote it; that observation is the diagnosis only. Every OS
    writes that reason into the handle's consumption marker, so a close interrupted after the
    marker and before the closed record keeps it. A committed marker is never rewritten, and a
    reason the closed record already carries, whole or as a `; `-joined part, is not added
    again. A downgrade to 0.4.49 in that window can lose the reason: its Windows replay reads
    none and rewrites the marker. Other closes keep their earlier outcome.
  - The close with a live owner, the cleanup after a failed binding and the read-only presence
    probe keep the identity without the launch label, as before.

  The branch table, the same on macOS, Linux and Windows (the attestation reads a birth and
  control identity on macOS and Linux, the recorded process identity on Windows):

  | Owner record and attestation | Running terminal | Finished | Absent |
  | --- | --- | --- | --- |
  | Bound, no other platform's fields anywhere in it, attested live incarnation (close with a live owner) | ended and removed when the full ownership proof succeeds; refused otherwise (for example a record with a matching birth but no TTY or process-group fields on macOS and Linux) | refused: the ownership proof needs a running terminal | Windows: refused (the proof fails before the adapter close); macOS and Linux: proven absence leads to the live owner's teardown, under its own checks |
  | Every other case: unreadable, none, damaged, contradictory, unbound, another session's, uninspectable process, exited between the two reads, PID-only live, gone | refused, nothing sent, handle kept | removed | settled |

  On the creation record, the cleanup after a refused reopen refuses every column before the
  core is asked; a whole creation record and the terminal's verified identity are required for
  every removal and settlement. On macOS and Linux a teardown intent left by an interrupted
  owner teardown keeps its own route while it validates; when the owner it names can no longer
  be established (its record gone, unreadable or not a record), the close takes the creation
  record, which confirms absence on the core.

  Handles created by 0.4.45 and 0.4.46 on macOS and Linux omit the nonce. For those, the
  terminal's launch label must be 32 lowercase hexadecimal digits and agree with the terminal's
  generated name. A failed read, kill or removal keeps the handle for retry. A refusal that
  names an hc command names the recorded endpoint; the user checks with
  `tabcli doctor <session> --probe` (doctor asks no core without `--probe`) that the core there
  is the one TAB recorded before changing anything. `terminal_surface_present` or
  `terminal_surface_missing` shows that the recorded core answered, and so does the
  `terminal_surface_unverified` detail of a terminal set to keep, which names the recorded core
  as having answered at its endpoint. The presence probe compares the argv, the creation time,
  the reported PID and, for a running program whose birth is observable, that birth; it does
  not validate the handle's creation record or the launch label. A handle the close refuses
  before the core lookup (no usable address or core, or a creation record that is not whole)
  is not recovered by ending or removing the terminal. Cleanup after binding fails uses
  the ordinary close proof. Cleanup before confirmation checks the full launch argv, label and
  name, plus the creation
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
  without opening another terminal. `ask` and `reopen` choose through one entry,
  `session_start::choose_launch_surface`: selection with the provider's pre-probe answer, then
  this admission; a reopen passes no provider, as it selects before reading its source. The
  provider's own admission follows in `resolve`. `doctor` reports `select_surface`, the
  selection without any admission, so it can name an automatic Herdr that `ask` then refuses
  (no verified caller, or a provider version below the Herdr floor).
  An explicit `--terminal herdr` can launch outside Herdr.
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
  re-check as the Linux host (`bound_host`), it flushes `CONIN$` (a
  console it cannot flush refuses the launch), keeps itself running through Ctrl+C and
  Ctrl+Break with a handler routine (not inherited, unlike ignoring them), and starts PowerShell
  7 from the core's `PATH` with the managed console root's exact arguments
  (`console_root_arguments`, which also removes `TERM`), through the helper command that drops
  caller markers. It exits with the root's code. The root shares the host's console and the
  core's job object, so `terminal.kill` (hc ends the job) ends the whole session.
- Each provider adapter answers whether it runs on a surface and returns its selected
  initial `Transport` (`surface_support`, required, no default), before anything is
  created (`provider::admit_surface`): first without a version
  before resolution and probing (and for terminal selection via `provider::surface_support`),
  and again with the probed version right after the CLI version probe, handing the
  admitted `Transport` straight into initial delivery. On Windows Herdr, Codex
  refuses immediately: its initial prompt is the injected console paste (#124), text without Return and one
  Return against the exact accepted screen, which `terminal.send` cannot do, and the
  launch-argument route used elsewhere would put the multi-line prompt on a Windows command
  line (32,767 characters; 8,191 and no line break through `codex.cmd`). Off Windows, Codex
  admits Herdr pre-probe and enforces stable 0.169.0 or newer once the version is known,
  refusing older or prerelease versions before any session or terminal exists. Claude (official
  messaging; its dialog is left to the user, as on macOS and Linux), Agy (paste through the
  surface behind its log readiness and receipt) and Pi run there. Inside an hc terminal the
  automatic choice is Herdr when the provider's adapter runs there and the managed console
  otherwise; a reopen, whose provider is unknown at selection, keeps the console.
- A Herdr handle on Windows never reaches the console control helper: the seven surface
  operations resolve its adapter through the one Windows lookup (`windows/dispatch.rs`), and
  guarded dialog input answers with Herdr's declared refusal first, as on the other platforms.
  Pi and Agy keep their Windows framing, readiness and receipts; only the transport is
  `terminal.send`.
  Agy's Windows readiness also needs `Full redraw completed`, measured on the console only;
  without it the paste is withheld and reported not sent.
- The Windows ownership proof checks the Herdr terminal on its core, then the owner's process
  identity. The owner's death proves nothing about the terminal, and Windows follows the
  rule above: Repair leaves a gone owner's Herdr terminal for `close-session --explicit`.
  0.4.49 and earlier closed it during Repair, including a running one, without the
  launch-label check and whatever the owner record said. The explicit close gathers the same
  evidence before the ownership proof, which a gone owner never passes, and closes by the
  creation record through the Windows lookup (`close_created_surface`). An attached `ask` or
  `tell` stops waiting once the owner is verified gone, as on macOS and Linux. A close that
  fails (no core, another core, a changed terminal) restores the handle and leaves state and
  claim for the next close. Only the core's `not_found` proves the terminal gone. The managed
  console keeps its own Repair: an active session's console is closed through its root
  identity, an ended one settles from its records. The owner's recorded incarnation differs in
  form by platform (a birth on macOS and Linux, the process identity on Windows); the rule
  reads only whether one is recorded.
- The PowerShell `attach_command` sets `HC_SOCKET` in its own script block and restores the
  earlier value (or its absence) in `finally`, so later commands keep their own core.
- Windows tests: endpoint resolution, the shared framing, the adapters' surface answers and the
  Windows lookup's routing and declared refusals over recording adapters run on every
  platform; the lookup over the Herdr adapter on a scripted core runs on macOS, Linux and
  Windows, including Repair with a scripted owner observation and the real close transaction.
  The gone-owner table (`herdr/tests/owner_gone.rs`) runs on every OS with that OS's close
  wiring; on Windows it drives the Windows decision, which runs on the Windows runner only.
  The pipe transport (a real pipe of this user), process births, parents, tokens, the installed
  lookup against an absent pipe, the directory admission, the Windows-only Repair tests,
  the attach line under PowerShell 7 and the
  binding wait run on the Windows CI runner. Live (reported by the
  verification session, 2026-11-18): the Windows PR head (505f0e7) passed the five native
  Windows self-test steps with Pi 1.5.0 on hc 0.1.21, Windows 11 x64, as a standard desktop
  user, with distinct turns and no terminal left after close; the release before it refused.
  Claude and Agy on Windows Herdr have no live run.
