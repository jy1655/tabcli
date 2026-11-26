# Current Attestation Boundary

- One rule decides whether a recorded incarnation is still the live process (`process_identity`):
  Live, Gone(Exited), Gone(Reused) or Unknown. Only the platform observer confirms absence: raw
  `ESRCH` on macOS, `ERROR_INVALID_PARAMETER` from `OpenProcess` or a signalled handle on Windows.
  An inspection failure of a process whose identity was recorded is Unknown. Never derive a death
  from the liveness bool when an identity was recorded: on Windows that bool turns every open
  failure but access denial into "dead". The bool answers only pid-only records.
- Unknown is not conservative by itself. Prune blocks, repair fails, the wait continues and the
  exact-intent close resume refuses. A dead owner must reach Gone through the observer, or every
  command on that session fails: "every unverified open failure is Unknown" would have made every
  dead Windows owner Unknown (plan defect found in review, 2026-10-15, #190).
- A reused pid is the verified death of the recorded incarnation and grants no authority over its
  surface. A changed control identity (controlling TTY, process groups) of a live incarnation is
  not a death. A second observation of the same pid is checked against the attested incarnation
  before its control fields are compared; a later snapshot with another birth is reuse (#190).
- The owner's rule is one behavior on every OS, with a difference only where it is unavoidable
  (2026-11-26). Repair treats Gone from exit or reuse the same, whatever the owner record's
  binding, and never ends a terminal whose core keeps it (Herdr): only `close-session
  --explicit` does, on every OS (os-parity). That close proves the terminal by its own creation
  record, so a reused PID or an unbound, foreign or incomplete record neither grants nor blocks
  the removal of a finished terminal. A Gone answer about another session's owner, an unbound
  owner or a PID-only record does not show that this session has ended. Ending a running
  terminal therefore also needs this session's owner positively gone. That means a bound record
  naming an incarnation the platform can produce (`records_incarnation`: a valid birth, a
  Windows creation time and path), observed exited, or reused by a process created after it
  (`reuse_corroborated`). Reuse classification compares for equality only, so an impossible or
  damaged birth reads as reuse; the corroboration does not. A PID-only Gone rests on the
  liveness bool and never ends one. The ordinary macOS and Linux
  dead-owner Close of the other kinds still requires an exited, bound owner with complete
  recorded control. Only the form of that identity is a platform fact (controlling TTY, process
  groups and birth on macOS and Linux, the recorded process identity on Windows).
- Startup repair consumes the typed owner attestation too: only Gone fails an unconfirmed
  launch as an exited wrapper. Live with changed or unknown control is still live; process
  Unknown or an unreadable owner returns an error without retiring the claim. A missing owner
  can still be registering. The launch deadline independently fails startup, without calling
  the owner observer or implying process death.
- Attestation reads the owner value and OS evidence only and takes no session lock; launch repair
  and close call it under their own lock. Callers (prune, repair, wait, close resume, the
  terminal adapters, reopen, the Claude registry) map the four answers in their own vocabulary
  and keep their messages and authority.
- Windows: the attestation compares executable paths case-insensitively; record and surface
  handle equality stay exact. The observer opens with `PROCESS_QUERY_LIMITED_INFORMATION |
  SYNCHRONIZE`, so a process that grants only query access is Unknown, never absent. The
  image-path query of an exited process fails (os error 31, GitHub runner, 2026-10-15), so an
  exited process whose object is still retained is Gone through the signalled handle (#191).

