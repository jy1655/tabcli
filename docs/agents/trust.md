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
  exactly, the consent of the main working tree of the repository that registered the
  workspace as a linked git worktree. That main working tree's consent is assessed exactly
  as TAB assesses an exact folder, with its own record: its pinned source, identity and
  revocation decide, never a provider store read past them. Its source provider must not
  decline, or fail to read, the worktree itself. A child or parent directory gets nothing.
- `consent/worktree.rs` establishes one property and nothing about Git's semantics: someone
  who can write a trusted repository's `.git` can register a worktree, and that worktree is
  then trusted (the owner's words, 2026-11-26). It runs no `git`, does not read or evaluate
  Git configuration and does not inspect `HEAD`, objects or refs, so a repository with
  `core.worktree` or unusual extensions is judged by its registration alone. Its steps:
  `W/.git` is a regular file `gitdir: <path>` (absolute, or relative to `W` as Git resolves
  it); lexically, with no I/O on it, the path claims `<main>/.git/worktrees/<id>`; before
  any I/O on `<main>`, consent's screen requires exact consent evidence for exactly that path
  (the evidence an exact folder needs), so an untrusted or unknown destination is never
  traversed; then the same path is walked component by component (each inspected before the
  next, none a link or, on Windows, a reparse point, every one a directory, `..` stepping back
  only over a directory just inspected); `<main>/.git` has no `commondir` of its own; and the
  registration's `gitdir` and `commondir`, walked the same way, name `W/.git` and
  `<main>/.git` by file identity, never by name. Relative-path registrations (Git 2.48
  `--relative-paths`) are resolved from the locations Git defines. The screen asks the
  providers' own readers; Claude's reader probes `.git` in the claimed path's ancestors and
  can run `git rev-parse` there before it reads its store, as it does for any exact folder
  (an open question to the owner, see the ticket report).
- `consent/authority.rs` is the one answer to "may this target provider treat this
  session's workspace as consented now". It reads the session's assessment as a typed record,
  binds it to the manifest's workspace by path and identity, assesses that workspace again
  without writing and without acquiring anything (a session's recheck needs the pins recorded
  at its start, so a `consent reset` of the worktree or of its main working tree refuses until
  a new `ask` or supported `reopen` acquires consent again), and asks the target provider
  whether it declines, or cannot read, the workspace or its main working tree. A record or
  manifest that cannot be read is an error; a target store that cannot be read is a refusal
  marked unreadable; anything else that does not hold is a refusal with its reason.
- The yes is consumed at the act, inside the step that performs it, never before a wait: a
  provider's launch option in its plan and again in the spawn fence, under the lifecycle lock
  immediately before the provider starts (`consent::admit_spawn`: an applied option must still
  have its own target's yes, bound to the workspace; another adapter's option or an unreadable
  record starts nothing); a guarded key inside the dispatch callback, after the pending
  transition (`workspace-dialog-response-pending`) was written conditionally on the applied
  value the step assessed; and an Agy paste immediately before the dispatch, after every
  readiness wait, with the customization-load line required in the same log contents that
  supply the receipt offset. An applied marker records what a launch did and is never
  authority: every gate takes the target's own marker from the one typed authority result, and
  the dialog step refuses another adapter's marker or a value it does not know.
- An inherited source records the main working tree's identity (`repository_root` in the
  shared record and the session's assessment). Who can make a worktree count: whoever can
  write the trusted repository's `.git` (to register it) and the worktree's own `.git` file;
  a cloned or fetched repository cannot, because Git never writes `.git/worktrees` from
  fetched content. What such a writer gains is what trusting the main working tree already
  accepts: any commit checked out there, including hooks and provider configuration in it.
  Codex applies the same inheritance itself (measured, above); Claude Code 2.1.332's bundled
  source resolves a worktree to its main root as well (read, not run).
- TAB writes no provider trust store. Consent is applied only through a provider's own
  process-local option, recorded as that adapter's applied marker (Codex `-c projects`,
  Pi `--approve`, Agy's `--settings` file), or, for exact consent where the target's adapter
  has no such option (Claude) or its launch recorded the option as unavailable
  (`launch-option-unavailable`: an Agy positively identified as older), by the guarded answer
  to the provider's own dialog, after which the provider saves its decision. An inconclusive
  launch (a version that identifies nothing, a refused or failing check) never falls back to
  the key. Inherited consent never takes a key: "Yes" would have the provider save the worktree
  as exact trust. Claude has no option, so with inherited consent it launches as with
  unverified consent.
- Stored data: a record without `repository_root` keeps the bytes earlier binaries wrote,
  and their meaning. An earlier binary (0.4.49) ignores the field of an inherited record,
  rechecks its source with the workspace's own key, finds none, and reports
  `source-reset` without writing; it refuses consent, never grants it. Its `consent revoke`
  drops the field and keeps the source, which then stays revoked; its `consent reset`
  clears both. The assessment at a session's start writes the main working tree's own record
  only when that consent is verified, as an `ask` there would. A session directory gains
  `workspace-consent.lock`, the session record's own lock; earlier binaries ignore it.
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

