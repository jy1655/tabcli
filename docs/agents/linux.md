# Current Linux Surface Boundary

- Linux supports tmux and a running local [Herdr Core](herdr.md). Inside an hc terminal,
  automatic selection verifies the caller on that core. Elsewhere it defaults to tmux.
  tmux requires version 3.2+ on an absolute PATH entry and uses its canonical path.
  WSL is not verified. The tmux server rules below apply only to tmux.
- One server belongs to one managed session (owner's decision). Keep its socket at
  `<session directory>/tmux.sock`; refuse an existing path or a path of 108 bytes or more
  before running tmux, suggesting a shorter state directory. No GUI opens in this release.
- Start detached with `-f /dev/null`, one tmux session named `agent-bridge`, and a 200x50
  window. Never load the user's tmux configuration. Drop `TMUX`, `TMUX_PANE`, and
  `TMUX_TMPDIR` from client commands so an outer tmux cannot become the addressed server.
  Naming the tmux session after the TAB id made Claude list the recipient twice on M8
  (2026-11-17, Claude 2.1.326); preserve the fixed session name.
- Bind pane, window, session, socket and server incarnation (pid plus boot id and procfs
  start ticks). Creation proves this boot and a birth no earlier than the clock reading
  just before `new-session`, with exactly one session and pane. Before every later operation,
  verify that recorded incarnation through a pidfd. Absence comes only from that incarnation
  being gone (exit, PID reuse, another boot); a live server whose socket refuses is an error.
- Before tmux starts a server, the launcher reproduces the pane host's session-directory
  admission for TAB's own account, which the server and its pane run as
  (`bound_host::refuse_before_launch`); a refusal is a failure to open the Surface. Herdr's is
  judged before the core is reached for what needs no account, and on the connection that
  creates the terminal for the core's account.
- The pane host waits for its exact binding, discards startup input, then runs
  `bash --noprofile --norc -m -c`. Keep job control and the wrapper's foreground process group.
  The same Linux bash handoff serves Herdr after checking its terminal address and host PID.
  No shell startup files run. tmux inherits the caller's environment, Herdr the core's.
  Both remove provider-private caller markers.
  Do not infer profile PATH additions or node availability from a provider launcher existing.
- Codex uses its queue and Claude official messaging. Agy/Pi paste into the exact pane via
  one uniquely named stdin-loaded buffer and raw `paste-buffer -d -r`. Before paste starts,
  failure is not sent; after it starts, failure is uncertain and never replayed. Viewer copy
  mode must not redirect input. `screen` captures visible application rows, not viewer scrollback.
- Never answer a dialog: tmux cannot compare the screen and send a key in one call. A
  Claude/Agy trust-dialog refusal keeps the pane for the user to attach and answer; the
  stopped session still needs explicit close. Neither consent nor bypass permits a key here.
- Explicit close sends SIGTERM through the pidfd verified against recorded birth, ends the
  whole session server (including user-added windows). Never unlink the socket: no check can
  prove the path still belongs to the ended server, and the stale file grants nothing. Repeated
  close and already-ended servers send no further signal. Pane hangup can take a few seconds
  to end owner/provider processes after close returns.
- Read owner identity from procfs: boot id plus start ticks, process group, session,
  controlling TTY and foreground group. A zombie retains its incarnation until reaped.
- Measure Linux provider commands before spawn: each argument or environment string including
  NUL fits `MAX_ARG_STRLEN` (32 pages, 131072 bytes on 4 KiB pages), and argv plus environment
  fits `ARG_MAX`. Refuse oversized commands with nothing sent, including Codex queue messages.
  Codex starts legacy notify itself, so TAB cannot premeasure that command. A 256 KiB
  context attachment can exceed a command budget; long material belongs at a workspace path.

