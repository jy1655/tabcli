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
  a core). Calls pass no `caller`, so TAB marks nothing seen for another terminal. On the
  Unix socket a read waits with `poll`, never a receive timeout: macOS refuses one (EINVAL)
  once the core has closed its end, which lost a reply already received (2026-11-26). It
  then receives without blocking and, with nothing to receive, waits again until the
  deadline: Linux reports a queue holding only an urgent byte as readable.
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
- The launch host runs as the core's account and refuses a session directory it does not find
  private to that account. The caller reproduces that verdict before any Surface exists
  (owner's decision 2026-11-26, option 3a): before the core is reached it judges what needs no
  account (`bound_host::refuse_before_launch`: an absolute path that can be sent as text
  exactly, a session-id name, a real directory, and on macOS and Linux the permission bits);
  on the connection that sends `terminal.run`, right before it (after the caller check, which
  can replace the connection), the adapter judges the rest for the core's account
  (`Core::admit_launch_directory`, `bound_host::refuse_before_core_launch`): on macOS and Linux
  the socket peer's uid, which the connection already required to be TAB's; on Windows the
  token user and default owner read from the pipe server's process handle that the user check
  opened and the connection keeps. A connection that cannot name the account leaves the owner
  and, on Windows, the access list to the host. A refusal names the host's reason and, when
  TAB's own account would be admitted, how the accounts differ. Observed 2026-11-26 (Windows
  11, hc 0.1.21): an elevated TAB (an administrator's SSH session) gave the session directory
  the Administrators group as owner, which the host of a core that is not elevated refused
  only in the launch log, and the caller timed out after 31.7 s. The caller refuses only what
  a read showed (an absent directory included): a read that fails otherwise, such as TAB's own
  token denied `READ_CONTROL` or a transient I/O error, refuses nothing, and the host decides
  as in 0.4.49. A refusal is an early decision (below) and settles as a failure to open the
  Surface (`launch::terminal_failure`). It is detected
  at once; returning it can wait for the lifecycle lock up to the launch budget. It is judged
  once, from what was observed: a directory corrected while TAB waits for the lock is not
  examined again, and the remedy is to correct the directory and run the command again. The
  caller's error names the directory and the host's reason. If the facts are not established
  (below), the refusal writes nothing: the status, the Claim and the launch log stay as they
  were (for example a `Launching` status and the `launch_pending` line TAB wrote when the launch
  began), and the returned error holds the refusal. On established facts the refusal settles the
  launch through the existing settlement (`fail_locked`, as in 0.4.49) with a fixed reason that
  names no path (`terminal launch failed: the Herdr launch host would refuse the session
  directory; no surface handle was recorded and no prompt was sent`): it writes the failed status
  with that reason, then releases the Claim, then appends the same line to the launch log. An
  error writing the status or releasing the Claim ends the settlement there and is returned with
  the refusal (`terminal launch failure handoff: ...`). The log append is best effort: its
  failure is not reported and changes nothing else. What the status becomes after a refusal that
  wrote nothing is left to the existing paths: `reopen` can copy the returned error into it, and
  repair can settle the launch first (below). No terminal exists either way. What the caller
  cannot know stays the host's, unchanged from 0.4.49: the directory changed after the check,
  a program started under another token than the core's own, a read only the host can make.
  The host logs its refusal as before when it still can (a directory that is gone takes no
  line), and the launch ends as below when its terminal finishes.
- The launcher's wait reads the launch's terminal through the Herdr adapter's own probe
  (`launch_surface`): the exact terminal with the presence proof, and the launch label when
  the handle records one. A terminal its verified core reports finished before startup was
  confirmed fails the launch at the next successful probe, on every platform, as a tmux
  surface that disappears does (owner's decision 2026-11-26, option A); this reverses, for a
  host that ended before startup, #409's choice to leave its admission, receipt, binding and
  Pending-launch refusals to the launch deadline. It is an early decision (below,
  `finished_before_startup`): a `Pending` receipt also means that no provider was started that
  could still confirm, so a `Spawning` receipt is left to its confirmation or its deadline. The
  ordinary poll's reading that it requires is read without its write: a launch-host Record's
  failure (`input_flush_failed`, `launch_shell_exec_failed`) is left to the ordinary poll to
  publish, and a Record that cannot be read is no evidence (`host_record_pending`). Then the
  launch fails through the deadline's settlement (`settle_locked`). A deferred decision writes
  nothing and returns the wait to its ordinary polling, as in 0.4.49, while the launch budget
  lasts, also when the receipt's deadline deferred it (that deadline is not the launch
  deadline). A terminal the core then reports absent is handled by the ordinary settlement
  (`settle`), as in 0.4.49: it keeps a confirmed or ended outcome it reads under the lock, can
  return a record error, and uses the disappeared-surface reason for a launch that is still
  pending and that it can settle. Finished-terminal probes that keep deferring leave the launch
  to the ordinary poll and its deadline. On a completed settlement the Session records the
  fixed reason `terminal surface finished before provider launch was confirmed`, and the error
  returned to the caller adds the core's report and the launch log's path, `(Herdr terminal t7
  exited with code N); see the launch log <path>`; nothing is read from the log. The exit code
  is the core's `exit_code` when it gives one (the field name is read from the installed hc
  0.1.22's string table, not from a live core, and nothing is decided on it). The settlement
  writes as the refusal's does (above): an error writing the status or releasing the Claim ends
  it there and is the wait's error, and the log append is best effort. When it completes, the
  launch ends failed, its Claim released, its receipt pending and its surface handle kept, so
  `close-session` removes the finished terminal (`terminal.rm`). Creation removes a created
  terminal that its last live read finds finished; one that finishes after that read is bound
  and left to this probe.
- Early launch decisions: the directory refusal before any Surface (every bound host) and the
  finished Herdr terminal above end a launch before its deadline. Failing at once is, by the
  owner's decision, a different outcome from waiting for the deadline whenever the world
  changes in between; the bar for both (2026-11-26):
  (a) An early decision is taken only from facts read under the lifecycle lock at the moment of
  the decision, about this launch's own records: the Claim at the path is this launch's token,
  the launch receipt carries the same token and is pending, the status is `Launching`
  (`pending::judge`), and the ordinary poll's reading says pending. Nothing is written,
  created or re-permissioned before that is established.
  (b) When any of those facts cannot be read, or is not as required, the early decision does
  nothing: the wait goes on to its next ordinary poll, and the existing paths decide (a refusal
  is returned without writing). An observation that is inconclusive is never evidence.
  (c) Once taken on those facts, the decision stands, even if the world changes afterwards.
  That is what "fails at once" means, and it is the owner's decision.
  (d) An early decision never acts after the launch deadline; from then on only the existing
  deadline path acts.
  One function establishes these facts for both decisions, in this order, writing nothing
  (`launch::establish_early_facts`): the existing lifecycle lock file, opened for reading and
  writing without creating or truncating it or changing its permissions, without blocking on
  a special file (macOS, Linux), and accepted only when the opened handle is a regular file
  (`Store::try_lock_existing`); one lock attempt; under it, this launch's Claim token at the
  path; the receipt of that token, still `Pending`, and the status `Launching` without a
  tombstone (`pending::judge`); the launch-host Record, read without the ordinary poll's
  publication of a recorded failure (`host_record_pending`; the refusal needs it too, because
  a macOS capture host's Record exists before the directory is checked); the ordinary poll's
  reading of the status and the receipt (`read_confirmation`); and last, the monotonic launch
  budget and the receipt's deadline. A read that fails or is inconclusive, a fact not as
  required, a busy or unavailable lock, or a clock that has run out establishes nothing. Each
  decision writes only on established facts, at once, under that same lock. Otherwise the
  finished-terminal probe defers to the ordinary poll, and the refusal returns the original
  refusal. At the deadline, a busy lock adds a lock-timeout diagnostic, including Claim
  retention only if this launch's Claim is still seen at the path.
  The reasons are in the failed `status.json` and, on a best-effort basis, in
  `launch.log`; `residual_surface()` and the status readers interpret the status. A later
  successful close without a terminal handle writes its closed record and status without that
  error. The returned refusal keeps the directory, the error of a completed finished-terminal
  settlement keeps the terminal and the launch log's path, and reopen's refusal record can keep
  that full error as its detail (`record_failed_launch`), as for a failed launch since 0.4.49.
  Reopen can copy that detail into the status error of a refused launch still launching with
  its Claim in two places: the original `reopen` command itself, which passes the detail to
  `fence_failed_start` once it gets the lifecycle lock (`fail_reopen_launch`, after
  `record_failed_launch`), for example when a reader held the lock through the refusal's
  deadline and then released it; and a later reopen of the same source
  (`retry_interrupted_launch_fence`, through `retry_failed_start_fence`). Whether reopen
  copies it is decided by `fence_failed_start_locked` when reopen holds the lock, not by the
  refusal or by lock contention: it can copy the detail when it finds `Launching` with the
  matching Claim and otherwise copies nothing, so a repair that settles the launch and
  releases its Claim first keeps its own failed status.
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
  reads (a finished terminal is only removed). Herdr's registration declares that its core
  retains a finished terminal (see [Close by creation record](#close-by-creation-record)).
  To remove a finished terminal on the creation record, TAB checks the launch label, sends
  one `terminal.rm`, and confirms absence on that core. It never sends `terminal.kill` on
  this path. Handles created by 0.4.45 and 0.4.46 omit the nonce; for those, the original
  identity checks also require the launch label to agree with the terminal's generated name.
  A failed read or removal keeps
  the handle for retry. Cleanup after binding fails uses the ordinary close proof. Cleanup
  before confirmation checks the full launch argv, label and name, plus the creation
  time and PID from its first read at that address. It refuses a terminal set to keep. A birth
  is needed to retain a handle for retry. The core has no conditional kill or remove, so another
  client of the same user can still change the terminal between TAB's last read and its action.
  hc does not sweep detached processes. TAB touches no other terminal and calls no core operation.
- An explicit `--terminal` wins. Otherwise `HC_TERMINAL_ID` with `TERM_PROGRAM=ghostty` (the
  identity hc gives every program) selects Herdr, with the terminal the platform chooses
  outside Herdr as its fallback (`terminal::Selection::Herdr`; Ghostty on macOS, from that
  same `TERM_PROGRAM`, tmux on Linux, the managed console on Windows); the marker under another
  `TERM_PROGRAM` was inherited and is ignored. This rule is the same on every OS (owner's
  decision 2A, 2026-11-26) and reads only the environment. `ask` and `reopen` select through
  one entry, `session_start::choose_launch_surface`, at the place where 0.4.49 selected and
  checked: `ask` after its attached results, `reopen` before it reads its source. There an
  automatic Herdr choice is checked against its caller (`terminal::admit_selection`), as in
  0.4.49, for every provider that can end on Herdr. A failed check refuses the launch there
  where the choice is already known to be Herdr: for `ask`, Claude, Agy and Pi; for `reopen`,
  which does not know its provider yet, always (every provider that supports reopen runs on
  Herdr, so no reopen that would take the fallback is refused). Where the version decides
  (Codex off Windows), the outcome is kept (`session_start::CallerCheck`) only while the
  request has time left. For `ask`, the pre-probe surface decision precedes workspace resolution.
  For `reopen`, the source workspace spelling is checked before child recovery, and other
  workspace errors are reported after the pre-probe surface decision. Both use the probed
  version in `resolve_at` (`session_start::admit_choice`). An automatic choice is Herdr where
  the adapter admits it and its fallback otherwise (Codex below stable 0.169.0 off Windows, any Codex on Windows), and an
  explicit `--terminal herdr` it does not admit is refused with the adapter's text. An
  automatic choice that ends on Herdr launches there only if its check passed, and is
  otherwise refused with the check's error; one that takes the fallback ignores the check.
  The automatic caller check shares the earlier of the request deadline and ten seconds across
  configuration discovery, connection, core requests and the walk of parent processes.
  The deadline bounds waits, although decoding a reply received just before expiry, a
  configuration read or a parent lookup already in progress can finish later.
  On Windows, a cancelled pipe operation may return later because the system must finish
  using its buffer before TAB releases it.
  On Windows, an automatic reopen inside hc checks its caller,
  where 0.4.49 chose the managed console without a check.
  That added check can also change which of two truthful contention verdicts a concurrent
  reopen gets, because the probes after it happen later.
  For `reopen`, the settlement of an interrupted launch failure of the source's last child
  (`retry_interrupted_launch_fence`) runs after the selection, the caller check and the
  lookup of the source directory, as in 0.4.49. Before this retry TAB checks the state root,
  executable and, when the manifest is readable, the source workspace for representable paths.
  A manifest read error is returned after the retry. The judgment of the source's marker, the
  other source gates, the provider's reopen support and the rest of the resolution, the
  consent assessment included, can still refuse afterwards and leave that child `Failed`, as
  in 0.4.49. The caller's terminal must be live on the addressed
  core with this process among its program's descendants, or the launch is refused without
  opening another terminal. Up to 64 parent links are compared; only a chain that ends at a
  root (no parent, or init) proves the command is outside, while reaching that bound or a
  parent that cannot be established (on Windows: it ended, its PID was reused, or it may not
  be inspected) is reported as unverified, and both refuse. So a caller that cannot be
  verified never refuses an `ask` on the fallback, on any OS. `doctor` reports the same
  decision (`session_start::automatic_surface`) without the caller check: see Observation and
  Self-Test Boundary.
  The fallback is not checked for presence before a session exists, as with any other
  terminal: a Mac without Ghostty, Linux without tmux 3.2 or Windows without PowerShell 7
  fails at that terminal's open with its own refusal.
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
  rule is unchanged). The caller judges the same rule for the core's owners
  (`consent::windows_directory_is_private_to`, read through `windows/process.rs`
  `QueriedProcess`); without the account it opens the directory for attribute access only.
  The core's elevation is read only to word a refusal. After the same binding wait, launch
  judgment and locked
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
  before resolution and probing,
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
  automatic choice follows the rule above: Herdr for them and the managed console for Codex.
  A `reopen` reaches that choice only for an eligible closed Claude source (Agy, Pi and Codex
  cannot reopen), which now gets Herdr after the caller check; 0.4.49 and earlier gave it the
  console.
- A Herdr handle on Windows never reaches the console control helper: the eight surface
  operations resolve its adapter through the one Windows lookup (`windows/dispatch.rs`), and
  guarded dialog input answers with Herdr's declared refusal first, as on the other platforms.
  Pi and Agy keep their Windows framing, readiness and receipts; only the transport is
  `terminal.send`.
  Agy's Windows readiness also needs `Full redraw completed`, measured on the console only;
  without it the paste is withheld and reported not sent.
- The Windows ownership proof checks the Herdr terminal on its core, then the owner's process
  identity. A console's ended session still settles from its records.
- The PowerShell `attach_command` sets `HC_SOCKET` in its own script block and restores the
  earlier value (or its absence) in `finally`, so later commands keep their own core.
- Windows tests: endpoint resolution, the shared framing, the adapters' surface answers and the
  Windows lookup's routing and declared refusals over recording adapters run on every
  platform; the lookup over the Herdr adapter on a scripted core runs on macOS, Linux and
  Windows, including Repair with a scripted owner observation and the real close transaction.
  The pipe transport (a real pipe of this user, and the account it reads for its server),
  the caller's check of a session directory for a core's account (elevated runner), process
  births, parents, tokens, the installed
  lookup against an absent pipe, the directory admission, the Windows-only Repair tests,
  the attach line under PowerShell 7 and the
  binding wait run on the Windows CI runner. Live (reported by the
  verification session, 2026-11-18): the Windows PR head (505f0e7) passed the five native
  Windows self-test steps with Pi 1.5.0 on hc 0.1.21, Windows 11 x64, as a standard desktop
  user, with distinct turns and no terminal left after close; the release before it refused.
  Claude and Agy on Windows Herdr have no live run.

## Close by creation record

- Dead-owner Repair preserves a Herdr handle for explicit close on macOS, Linux and Windows
  (`repair_leaves_a_gone_owners_terminal_for_an_explicit_close`).
- Without a [verified live owner](attestation.md), an explicit close acting on a whole creation record
  removes only the finished terminal the verified core identifies, settles the session on
  that core's `not_found` for the recorded address, and refuses a running terminal.
- On macOS and Linux, a saved teardown intent must match the session and exact handle and
  give a nonempty reason. Its saved owner must be bound to the session, have a complete
  control record and a nonempty TTY, and lead the saved foreground process group
  (1 through `i32::MAX`). Replay with a live attested owner requires that same owner in
  the intent. Without a verified live owner, explicit close uses the creation record.
- An unreadable close-intent file refuses a close that needs an intent decision.
- Cleanup after a refused reopen refuses the creation record selected for lack of a live owner
  (`a_cleanup_never_acts_on_a_terminal_without_a_live_owner`).
- Each kill or removal requires a grant for the terminal observed on the verified core
  (`a_grant_names_the_core_that_reported_its_terminal`, `a_grant_names_the_address_its_basis_records`).
- A creation-record close chosen for lack of a live owner preserves the stored diagnosis
  across interruption
  (`an_interrupted_close_keeps_the_diagnosis_across_its_consumption_marker`).
