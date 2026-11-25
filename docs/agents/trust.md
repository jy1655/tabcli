# Trust Store Boundary

- A provider trust store, and a TAB consent record, is evidence only when nobody but
  the user can change it. On Unix that is the owner and the mode. On Windows it is the
  owner and the access list, both read from the handle that the content is read from: the
  owner must be the token's user or the token's default owner (the Administrators group
  for an elevated process, which is what a GitHub Windows runner is), and no access rule
  may let another account write, append, or change the access list or the owner. The
  system, the Administrators group and `OWNER RIGHTS` may. A missing access list and a
  rule that is neither a plain allow nor a plain deny are refused.
- The check describes the store at the time of the read. It cannot tell who wrote the
  content earlier, and it does not exclude a writer that has the store open. Do not open
  a provider store without write sharing to change that: TAB polls Agy's store every
  100 ms while Agy saves an approved decision, and a provider's own write would fail.
- The gate that withholds an initial paste on native Windows asks another question than
  workspace consent: not whether the user approved this exact workspace, but whether the
  provider takes input now. For Codex an exact project entry is not the only way past
  the trust dialog. Codex CLI 0.159.3 takes the trust of a workspace without an exact
  entry from its repository root, found by reading `.git` itself and checking a linked
  worktree's registration; it shows no dialog then and saves no exact entry, and "Trust
  and continue" in a subdirectory saves the repository root (measured 2026-10-02;
  `--yolo` does not suppress the dialog). A wait for the exact entry never ended there
  (issue #60). The Codex adapter therefore also opens the gate on positive evidence from
  the screen: the empty composer row, which Codex draws only when no onboarding screen
  and no view of the bottom pane is active. Do not infer the absence of the dialog from
  Codex's configuration, from Git, or from a screen that merely lacks the dialog's
  words: an empty screen is not evidence, and a wrong inference pastes an Enter onto
  "Trust and continue". Consent that is shared with another provider stays exact, with
  the one worktree rule below.
- Consent has one rule for every provider (owner's decision, 2026-11-26): exact trust of
  the workspace in a provider's own store, or, only when no provider trusts the workspace
  exactly, exact trust of the main working tree of the repository that registered the
  workspace as a linked git worktree. `consent/worktree.rs` reads Git's files and runs no
  `git`: the workspace's `.git` is a regular file naming `<common>/worktrees/<id>` by an
  absolute path, that registration's `commondir` is Git's `../..` and its `gitdir` names
  the workspace's `.git` back, no path on the way has a link (on Windows, any reparse
  point), the common directory is the `.git` directory of its parent (which rules out a
  submodule and a bare repository), and its `config` and `config.worktree` neither set
  `core.bare` nor `core.worktree` nor include another file. Git's relative-path
  registrations (`worktree.useRelativePaths`) are not followed. Anything else is no
  worktree and keeps the exact rule. A child or parent directory still gets nothing.
- An inherited source records the main working tree's identity (`repository_root` in the
  shared record and the session's assessment). Every recheck verifies the registration
  again, the main working tree's identity, and the recorded store's exact entry for it;
  any change is `source-reset`, with no substitution, exactly as for an exact source. A
  target provider that declines or cannot read the workspace or that main working tree
  takes no consent. Who can make a worktree count: whoever can write the trusted
  repository's `.git` (to register it) and the worktree's own `.git` file; a cloned or
  fetched repository cannot, because Git never writes `.git/worktrees` from fetched
  content. What such a writer gains is what trusting the main working tree already
  accepts: any commit checked out there, including hooks and provider configuration in
  it. Codex applies the same inheritance itself (measured, above); Claude Code 2.1.332's
  bundled source resolves a worktree to its main root as well (read, not run).
- TAB writes no provider trust store. Consent is applied only through a provider's own
  process-local option, recorded as that adapter's applied marker (Codex `-c projects`,
  Pi `--approve`, Agy's `--settings` file), or, for exact consent only, by the guarded
  answer to the provider's own dialog, after which the provider saves its decision. A
  launch that recorded an applied marker, and any launch with inherited consent, sends no
  key: "Yes" would have the provider save the worktree as exact trust. Claude has no such
  option, so with inherited consent it launches as with unverified consent.
- Stored data: a record without `repository_root` keeps the bytes earlier binaries wrote,
  and their meaning. An earlier binary (0.4.49) ignores the field of an inherited record,
  rechecks its source with the workspace's own key, finds none, and reports
  `source-reset` without writing; it refuses consent, never grants it. Its `consent revoke`
  drops the field and keeps the source, which then stays revoked; its `consent reset`
  clears both.
- A wait for workspace trust ends when its session has ended, and looks at the session
  before it looks at the evidence: the launcher of a closed session must not wait until
  its deadline, and an ended session is never pasted into. This holds for the Codex wait
  and for the Agy wait.
- Create a store fixture in a test the way a private record is created
  (`write_private`, `write_json_atomic`). A plain write inherits what the temporary
  directory allows, and on a PC with the Codex sandbox that directory lets another
  account modify its files (observed 2026-10-02: `CodexSandboxUsers`), so the fixture is
  refused there and accepted on CI.
- Every session start assesses workspace consent for the new session from the current
  stores, `reopen` exactly as `ask`: the source session's assessment is diagnostic history
  of another session and is never reused as consent (#202). A start that skipped the
  assessment would leave `authorized` false for good, so the session could not take the
  guarded dialog answer that a fresh `ask` can. The assessment is part of the resolution,
  before anything is created, and create records it with the session before anything else
  binds to that session: a failed assessment consumes nothing, and a record that cannot be
  written fails the creation, which for a reopen releases the source reservation (found in
  review, 2026-10-17).

