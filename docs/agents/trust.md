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
  owners (`windows_directory_is_private_to`): the caller judges a Herdr session directory for
  the core's token user and default owner before it creates the terminal, to reproduce the
  verdict of the host, which runs as the core; a read that TAB's own token is denied leaves
  the directory to the host. An elevated process's default owner is normally
  the Administrators group (a token's default owner can differ by policy or per process), so a
  directory an elevated TAB creates is normally not private to the user of a core that is not
  elevated. The rule itself is unchanged.
- TAB treats its state root as the user's own, as it did before this check: at every session
  creation it sets the root's mode to 0700 (macOS, Linux) or its access list to the user alone
  (Windows), and fails where it cannot. It does not verify that nobody else can change the
  root: on macOS an extended ACL entry survives the mode change, and on Windows the owner is
  neither checked nor changed.
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
  any I/O on `<main>`, consent's screen asks each provider, in the order of the exact sources,
  whether its own store lists exactly this path as trusted, answered from the store alone, with
  no I/O on the path or its ancestors and no `git`, so a destination no store lists is never
  touched; a path that names a component and later cancels it with `..` is refused lexically,
  before any I/O (only the leading parent steps of a relative path are taken); then the same
  path is walked component by component (each inspected before the next, none a link or, on
  Windows, a reparse point, every one a directory); `<main>/.git` has no `commondir` of its
  own; and the registration's `gitdir` names `.git` in `W` itself, first lexically (in a spelling
  Git writes, absolute or relative to the registration, before anything on its path is walked,
  so a stale registration naming a former worktree in a third tree is refused without touching
  that tree) and then, walked the same way, by file identity, the file and its directory, so a
  hard link to `W/.git` in another folder is not `W`; its `commondir` names `<main>/.git` the
  same way, first lexically (Git writes `../..` or an absolute path), so a stale `commondir` into
  a third tree is refused before any I/O there, and then by identity. A registration whose
  recorded spelling of the worktree differs from the worktree's current path, including by case
  only on a case-insensitive volume, does not inherit (a documented limit, router's ruling,
  2026-11-26): the launch falls back to the provider's own trust dialog, the safe direction,
  and distinct case-sensitive siblings stay refused. Deciding a volume's case rules would need
  I/O that the lexical rule avoids. The remedy is Git's own: `git worktree repair <worktree>`
  rewrites the registration of a moved worktree to its current path, but on a case-insensitive
  volume (`core.ignorecase=true`) it compares paths without case and keeps a case-only
  difference; `git -c core.ignorecase=false worktree repair <worktree>` rewrites that one too
  (measured with Git 2.54 on macOS, 2026-11-26). On Windows an identity is
  the volume and the full 128-bit file id (`FileIdInfo`; the 64-bit index can repeat on ReFS),
  and where it cannot be read nothing is inherited. Pointer files are opened so that a FIFO
  cannot block the open (`O_NONBLOCK`), and the opened handle is checked. Relative-path
  registrations (Git 2.48 `--relative-paths`, and the registration's own `commondir`) are
  resolved from the locations Git defines. On Windows an absolute path
  is rebuilt on its volume's root as `canonicalize` spells it (`\\?\C:\` with the drive letter
  in upper case, `\\?\UNC\server\share\`), the spelling a relative path inherits from the
  canonical workspace, so the screen is asked with the path an exact-folder lookup would key
  (`native_key` gives `C:\...` or `\\server\share\...`) and every walk addresses the file
  system without Win32's rewriting of names; a device or object-name root is refused, and so is
  a name that holds `/` (a verbatim path does not split there). Tests that write a pointer with
  `..` spell it as text: `Path::join` onto a verbatim root removes `x/..` lexically before the
  file is written (Windows CI, 2026-11-26).
- Whom the worktree check defends against (router's limit, 2026-11-26). The check establishes
  the owner's property (only a writer of the trusted main working tree's `.git`, together with
  a writer of the worktree's `.git` file, can make a folder count) against whatever content the
  launch folder holds, and against any local user who cannot write the main working tree's
  `.git` or an ancestor directory of either tree. It does not defend against someone who can
  replace those ancestors while the check runs; that is the same as for an exactly trusted
  folder, whose trust is by path. Within that limit it inspects by path: traversal by directory
  handles is beyond the limit and is not built.
- The screen is a necessary condition only, never consent (router's ruling, 2026-11-26):
  after it passes and the registration is verified physically, the main working tree's own
  consent is assessed by the full readers exactly as an exact folder is, and a path that a store
  lists but its reader then refuses gives no inheritance. Each adapter states its store-only
  answer (`store_lists_trusted`, no default body). Codex, Agy and Pi answer with their full
  lookup, which resolves the store from the caller's home, reads only that file and compares the
  path (and, for Pi, its lexical ancestors) as string keys. Claude's full lookup first probes
  `.git` in the path's ancestors and may run `git rev-parse` in the path, so Claude answers with
  its store-only body (`claude/trust.rs` `store_trust`). A reader that touches the path must
  never answer the screen: the full readers would let a hostile `.git` file make TAB stat
  ancestors of an untrusted share or run `git` there before anyone trusted it. Without a pin
  every provider's store is asked; a pinned source is screened in the one store its pin names,
  whatever stores the caller's environment names now, so a later command with another provider
  home keeps a valid pin (review r3, finding 9). Inheritance uses each provider's own key rule,
  the one its exact-folder lookup uses: Codex folds case on Windows, so in a case-sensitive
  Windows directory two sibling repositories whose names differ only in case share its entry
  (follow-up, below). The screen describes only itself: the launch folder's own exact-folder
  assessment still runs every reader on the launch folder as 0.4.49 does, and Claude's reader
  there runs `git rev-parse` in the launch folder when a repository encloses it, which follows
  the folder's own `.git` pointer (follow-up, below).
- Follow-ups beyond the two limits or outside this change (reviews r3 and r4): (1) recheck
  consent inside the terminal backends after their internal waits; (2) traverse the worktree
  paths by directory handles, and bind the child's working directory to a directory handle;
  (3) Claude's scope probe on the launch folder (present in 0.4.49): screen a worktree pointer
  before a scope probe may follow it, use the store-only result to skip Claude's
  positive-source scope probe when Claude's store has no entry, and keep the full scope check
  for a prospective Claude grant; and Codex's case-folded Windows key: for inheritance, keep
  the store's actual matched spelling and require an unambiguous match to the physical main
  working tree before the first pin, without changing Codex's ordinary launch.
- `consent/authority.rs` is the one answer to "may this target provider treat this
  session's workspace as consented now". It reads the session's assessment as a typed record,
  binds it to the manifest's workspace by path and identity, assesses that workspace again
  without writing and without acquiring anything (a session's recheck needs the pins recorded
  at its start, so a `consent reset` of the worktree or of its main working tree refuses until
  a new `ask` or supported `reopen` acquires consent again), and asks the target provider
  whether it declines, or cannot read, the workspace or its main working tree. A session
  record or manifest that cannot be read is an error, and so, as in 0.4.49, is TAB's own shared
  consent record (`workspace-consent/<hash>.json`, the workspace's or its main working tree's)
  that cannot be read or decoded, and a workspace that cannot be identified (review r5, finding
  2). Provider and Git evidence that the recheck cannot read (the pinned source's store, a
  required read of the registration or of the main working tree; a missing file is a fact: the
  worktree is no longer registered) and a target store that cannot be read are refusals marked
  unreadable (review r4, finding 1): a consumer that does
  not need the cause treats them as a missing consent was treated in 0.4.49 (Codex and Pi plan
  no option, Claude's dialog step sends no key, the Codex wait still tries its independent
  evidence), Agy's paste gate and doctor report the cause for a session that relies on the
  session-only setting (a session whose workspace Agy's own store lists exactly takes that entry
  first and does not ask), and a command that carries a grant refuses to start. A
  start records a reset for an unreadable source, as every earlier release did. Anything else
  that does not hold is a refusal with its reason. The main working tree is identified once per
  assessment, and a failure of its assessment is classified where it arises (review r6, finding
  1): when the verifier's own reads (its identification and alias check) find the folder gone,
  also after it was identified, the inheritance is withdrawn like a missing registration (a
  refusal: the consent no longer holds); a folder they cannot inspect is unreadable evidence (a
  refusal marked unreadable); TAB's own shared record of it keeps its error. A folder that goes
  after the verifier's reads is not seen by that recheck, and the next one finds it gone, unless
  a pinned source's own reader touches it first. That reader is the source's evidence, not the
  verifier's: Claude's reader looks for `.git` in the directories above the folder and runs `git
  rev-parse` in the folder only when one holds it (its scope probe, unchanged), and a failure
  there, also one caused by the folder going, is the source's trust that cannot be read, a
  refusal marked unreadable (review r7, finding 4). Each refusal withholds the grant and the
  key, and a plain command still starts. What Agy reports depends on its route (review r8,
  finding 3): Agy's own store entry for the exact workspace is read first, so a session whose
  workspace that store lists keeps its pastes, and doctor reports `agy_workspace_trusted` once
  the session's load line is there; only a session that relies on the session-only setting has
  its paste withheld, and doctor reports `agy_workspace_untrusted` for consent that no longer
  holds and `agy_trust_evidence_unreadable` for a refusal marked unreadable. Agy's marker read
  decodes the session record as its typed assessment (`session_applied`): valid JSON of another
  shape is unreadable there,
  never an absent marker. The Codex wait keeps its existing reading: `observe` only selects
  whether to ask the typed authority, and a record that supplies no marker leaves it to its
  independent readiness evidence.
- When the authority is asked (router's limit, 2026-11-26). Consent is asked again at the last
  point the provider-neutral code controls, and a change after that point takes effect for the
  next act, not the one in flight:
  - An Agy paste that relies on the session-only setting (a session whose workspace Agy's own
    store lists exactly is checked against that store at the same point, without the authority):
    in the delivery step, after every wait, immediately before the terminal backend's send is
    called, with the customization-load line required in the same log contents that
    supply the receipt offset. The terminal backends are not changed: a revocation that lands
    after that point does not stop the paste already handed to the backend (tmux's pane check
    and `load-buffer`, Herdr's target lookup and the Windows console helper's screen read run
    after it; review r3, finding 3, beyond the limit). The trace's paste timestamps surround
    the send itself and stay empty when the check refuses before it.
  - A guarded key: inside the dispatch callback, after the pending transition
    (`workspace-dialog-response-pending`) was written conditionally on the applied value the
    step assessed, under the session record's lock taken with the step's remaining budget (its
    expiry is proven unsent). Legacy eligibility comes from what the launch recorded, never
    from the pending value: a pending value that a step reads before its own write is not its
    own and refuses (another attempt owns it, or its key may have been sent), and every path
    that proves the key unsent (expiry, a failed liveness or authority read, a refusal, a
    changed screen, a stale claim) returns the record to the assessed value. If that
    restoration itself fails (the lock held until the deadline, the record not writable), the
    step stops and reports both facts, that the key was not sent and that its marker could not
    be restored; a later attempt then refuses as for an unowned pending value. A restore that
    finds the value already restored goes on; one that finds a value another writer put there
    keeps it, stops, and reports that the key was not sent and the marker was changed by
    another writer. After the key, the provider's confirmation is recorded only within the same
    deadline, checked before the write and again once the lock is held, on the step's clock and
    with the same boundary (a reading at the deadline is late). A confirmation is observed when
    the store read that shows it returns; its time is taken then, before the liveness check that
    follows (review r7, finding 3). A confirmation first observed at or after the deadline
    reports that the provider's trust was observed confirmed after the deadline, that the marker
    was not recorded and that no input was retried; a confirmation observed in time that the
    record cannot take in time (the lock held until the deadline, or the deadline reached once
    it is held, also during that liveness check) reports that the provider's trust is confirmed
    and the recording failed, with that cause (review r6, finding 4). The pending marker stays,
    and no input is retried.
  - A provider's start: the authority is asked only for a command that carries a
    trust-bearing option. The plan carries that option, its provider and the workspace identity
    it was granted for (`LaunchGrant`), and for an option that names a file (Agy's settings)
    that file's path and digest, to the spawn fence, which refuses the command unless the
    provider store reader (`read_store`) accepts the file at that path and its bytes have that
    digest. The reader refuses a final path component that is a symbolic link or not a regular
    file (the directories above it are not inspected), more than 8 MiB or text that is not UTF-8,
    on Unix a file that the effective user does not own or whose mode has a group or other write
    bit (owner and mode bits only: an access control list, such as a macOS ACL entry, is not
    read), and on Windows a file that fails the owner and access-list rule above. A replacement
    at the same path with the same bytes that the reader accepts passes; a refusal by the reader
    names the file and the reader's reason (review r7, finding 5; review r8, finding 4). The
    fence also asks about that command
    (`consent::admit_launch`) under the lifecycle lock immediately before its existing
    deadline check, so the order (deadline, `Spawning`, spawn) is unchanged; for a launch with a
    receipt the question is bounded by the receipt's remaining time: no time left refuses a
    granted command without asking, one absolute deadline is taken before the question starts,
    and an answer that arrives after it refuses, so it cannot push a start past the receipt's
    expiry (a launch without a receipt, which the fence judges on its status alone, asks without
    a bound). The grant is
    made only for the workspace the command was built for (the wrapper's snapshot that gives it
    its working directory and its option), and the fence requires the authority to find that
    same identity consented. At the spawn fence a command without such an option follows
    0.4.49's admission exactly and reads no consent there: a damaged record does not stop a
    plain Claude launch, a supported reopen, or a plain Codex or Pi command (every `ask` and
    supported `reopen` still assesses consent while the session is resolved). A change of
    consent during the
    durable `Spawning` write that follows, and the child's working directory between the check
    and the exec, are beyond the limit.
  An applied marker records what a launch configured and is never authority (after a failed
  grant whose restoration also failed it can name an option that attempt was not granted; review
  r8, finding 2): every gate takes the
  target's own marker from the one typed authority result, and the dialog step refuses another
  adapter's marker or a value it does not know.
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
  `workspace-consent.lock`, the session record's own lock, and Agy's settings files,
  `agy-session-trust-<random>.json`: one per granted plan, and any that a plan left when its
  removal failed or its wrapper stopped before the grant; earlier
  binaries ignore both, as they ignore every file they do not name, and their `prune` removes
  the whole session directory with them.
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

