# Trust Store Boundary

- A provider trust store, and a TAB consent record, is evidence only when nobody but
  the user can change it. On Unix that is the owner and the mode. On Windows it is the
  owner and the access list, both read from the handle that the content is read from: the
  owner must be the token's user or the token's default owner (the Administrators group
  for an elevated process, which is what a GitHub Windows runner is), and no access rule
  may let another account write, append, or change the access list or the owner. The
  system, the Administrators group and `OWNER RIGHTS` may. A missing access list and a
  rule that is neither a plain allow nor a plain deny are refused.
- A bound launch host's session directory follows the same rule with the directory-changing
  rights added (`windows_directory_is_private`). The rule can be judged for another process's
  owners (`windows_directory_is_private_to`): the launcher judges a Herdr session directory for
  the core's token user and default owner before it creates the terminal. An elevated process's
  default owner is the Administrators group, so a directory that an elevated TAB creates is not
  private to the user of a core that is not elevated. The rule itself is unchanged.
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
  "Trust and continue". Consent that is shared with another provider stays exact.
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

