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
- Repair treats Gone from exit or reuse the same, whatever the owner record's binding.
- For a Herdr close without a saved teardown intent, only a session-bound, platform-consistent
  owner record with an attested live incarnation selects the close with a live owner (see [Herdr close](herdr.md#close-by-creation-record)).
- The ordinary macOS and Linux dead-owner Close of other kinds requires an exited, bound
  owner with complete recorded control. For terminal kinds other than Herdr, the platform
  differences remain and are tracked in [issue #421](https://github.com/jy1655/tabcli/issues/421).
- While a launch is pending, the registered owner is visible to startup repair before the provider version check.
  The spawn fence binds the owner record to the spawning wrapper under the lifecycle lock.
  Once an owner is recorded, only that wrapper can settle its launch and release the Claim.
  Before registration, a wrapper error settles its pending launch only when the lifecycle lock
  confirms the absence of both an owner and a spawn attempt.
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

