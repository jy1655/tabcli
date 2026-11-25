# Current Windows Surface Boundary

- Codex owns injected paste evidence and guarded submission; see Current Codex Boundary (#124).

- The seven surface operations on Windows (open, send, screen, guarded key, verify, close, in
  its ordinary form or by creation record once the owner is gone, and presence) resolve their
  adapter through one lookup (`windows/dispatch.rs`), as `posix.rs`
  does on macOS and Linux: the Herdr adapter for a Herdr terminal, and `ManagedConsole`
  (`windows/mod.rs`) for every other kind. Screen and guarded key first answer with the kind's
  declared refusal, if it has one. A handle of a kind Windows does not run stays with the
  managed console, whose open refuses the kind and whose helper refuses the handle; refusing
  it in the lookup would change what such a handle reports and is not decided. Give a new
  Windows surface its own adapter in that lookup; do not branch one of these operations on the
  kind. `attach_command` and `admit_selection` still check kinds outside those operations.
  Dead-owner Repair decides whether it needs a close transaction in
  `terminal::close_authority::owner_exit`. Only a managed console reaches the Windows close
  adapter from that transaction: a registration whose core keeps a finished terminal (Herdr)
  is left for `close-session --explicit` on every OS (os-parity, 2026-11-26). The claimed
  handle is checked independently of the retained-handle scan. The explicit close of a gone
  owner's Herdr terminal goes through the lookup's close by creation record, which refuses
  every kind whose core keeps no terminals before its adapter. The lookup's portable tests run
  on every CI target; the installed lookup, over the console helper and the core's pipe, is
  tested on the Windows runner only.
- A Herdr terminal is not a managed console: its handle never reaches the console control
  helper, the tab host or the focus guard (see Current Herdr Surface Boundary). Its launch
  host (`windows/launch_host.rs`) starts the same PowerShell root through
  `console_root_arguments`; change that root in one place for both surfaces.

- A managed console on native Windows is a tab of Windows Terminal, and a console window
  of its own only when no tab can be had: `wt.exe` is not on an absolute `PATH` entry, the
  desktop has no shell window, a path is one that Windows Terminal would alter, or no tab
  host offers a root in time. The request for the tab is withdrawn before the console
  window is created, and a surface is bound only once, so a session never runs in two
  surfaces; an empty tab can exist beside the console window until its host notices.
- `wt.exe` creates the tab's process itself. The tab therefore runs `native-console-host`,
  which creates the same PowerShell root, suspended, in the tab's console, offers its
  attested identity through the session directory, and starts it only after the launcher
  has bound the surface. Keep the root, its command line and its environment identical to
  the console-window path, so that no control path depends on the host. Replace the host
  when Windows Terminal can adopt a process that its caller created.
- The tab host creates the root inside a job that ends it with the host, and releases
  the job only after it has started the root, so a host that dies before the root runs
  leaves no process. Whether the root is started is decided once, by whoever creates
  `console-host-decision` first: the host to start it, or the launcher to give up an
  offer it had accepted. Keep that decision atomic. A launcher that decides first ends
  the suspended root. A launcher that finds the host's decision ends nothing: the
  surface counts as started and stays bound, the launch goes on to wait for the
  provider, and a launch that fails there is fenced as on every platform. Do not end a
  console during startup with the close helper: it ends the processes it saw, and a root
  that has just started is about to start more. Never end a started root alone: a later
  close finds the console by its root. A root that was never started has reached no
  console, so a close cannot attach to it; the close ends it alone only when the
  session's own records show that the wrapper never ran (no owner record, no spawn
  attempt in the launch receipt).
- By default the tab goes to the window named `agent-bridge`, never to a window the user
  works in. Windows Terminal selects a new tab, cannot create one unselected, and offers
  no way to select the earlier tab again, so a tab in the user's window takes the keyboard
  inside that window for good (measured 2026-10-01, Windows Terminal 1.24). Only the
  user's own `settings windows-tab-window current` sends the tab to the most recently
  used window (`-w 0`). Do not change the default, and use the dedicated window when the
  settings record cannot be read.
- A new surface gives the keyboard back. Windows Terminal honours a show-without-activating
  request only for a window that `wt.exe` creates while another application is in front;
  it brings an existing window to the front whenever a command line is dispatched to it,
  and creates a console handed to it as the default terminal in front when one of its own
  windows already is. Give the foreground back only from the window that hosts the
  surface, and never guess that window: a process inside the console reads it from the
  console's own window (the tab host before it starts the root, the `window` control
  helper for a console window, whose console window belongs to the terminal's console
  host and not to the root). A terminal window that merely appeared during the launch may
  be one the user opened. A window the user goes to during the launch is remembered and
  gets the keyboard back from then on. The surface's own window keeps the keyboard when
  it comes to the front again later than a window does on its own (200 ms after the
  keyboard was given back; 54 ms was measured): the user selected it. Earlier than that
  the launch cannot tell the user from the window and gives the keyboard back, because
  leaving it in a session the user did not choose is the worse mistake. The foreground
  is watched by a thread of its own, so that this time is measured while the launch
  waits on files and processes. Never join that thread without a bound: giving the
  foreground back goes through the input queue of the window that holds it, and a window
  that has stopped answering can hold the call for good. The tab host discards the
  console input typed before it starts the root: an Enter would answer the first dialog
  the provider shows. A console window has no host, so a key typed while its window is
  in front reaches the console.
- Windows Terminal keeps a tab whose own process ended with a failure code. The tab host
  leaves the tab's console once the root has attached to it, and confirms the start only
  then, so that closing a session, which ends its console processes with a failure code,
  still closes the tab. A close that finds the host still in the console ends it last
  and without a failure code; it recognises the host by the pid and identity that the
  host records about itself in the session directory (`console-host-process.json`). The
  host offers no root before it has written that record, and a close that finds the
  record but cannot read it ends nothing. A host that is still there a second after the
  root has ended is stalled; the close ends it, without a failure code, so that no tab
  outlives its session.
- A close ends the console processes it found when it attached. A process that the
  session starts while it is being closed can survive the close; this is unchanged and
  holds for both surfaces.
- Windows refuses to replace a file while any other handle to it is open, and a launcher
  polls the records that a wrapper replaces. Replace such a record only through
  `persist_record`, which repeats the rename for a bounded time.
- The console launch passes its command to PowerShell as one double-quoted `-Command`
  argument. The bootstrap must not contain a double quote: 0.0.8 put one there, and every
  launch on native Windows was refused before a console existed. The Windows test that
  runs the bootstrap through the launch's own command line guards this.
- A full-width character fills two console cells and is returned once, so a screen row
  can hold fewer characters than cells. Do not treat a short row as a failed read.

