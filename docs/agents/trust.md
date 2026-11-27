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
  "Trust and continue".
- When acquiring a source, TAB considers inheritance only if no provider trusts the workspace exactly
  (`exact_trust_comes_first_and_consent_extends_to_no_child_or_parent`).
- A linked worktree can inherit the consent of its main working tree when Git's registration
  identifies both trees and the main tree's pinned source, identity and consent still hold
  (`a_verified_linked_worktree_inherits_the_consent_of_its_main_working_tree`).
- Records without `repository_root` keep their earlier bytes and meaning, and an earlier release
  checking an inherited source pin reports `source-reset` and never grants it.
- A registration under another spelling of the worktree path, even one differing only in case,
  does not supply inherited consent; use `git -c core.ignorecase=false worktree repair <worktree>`
  to repair the registration.
- `tabcli consent revoke` or `tabcli consent reset` on the main working tree withdraws TAB's
  inherited consent and leaves providers' own trust and independent exact consent intact.
  After a reset, a new `ask` can acquire consent again.
- The worktree check verifies the registration by path, without directory-handle traversal,
  against any content of the launch folder and any local user who cannot write the main working
  tree's `.git` or an ancestor directory of either tree.
- The worktree verifier may inspect a claimed main tree only when a provider store lists it
  (`the_claimed_main_working_tree_is_screened_before_any_io_on_it`).
- A session recheck needs the original consent pins and acquires none
  (`a_recheck_requires_the_pins_and_acquires_nothing`).
- Inheritance for a target requires readable trust decisions with no decline from the source
  for the worktree or from the target for either tree
  (`a_target_that_declines_the_worktree_or_its_main_working_tree_refuses`).
- Consent is checked at the last point provider-neutral code controls for each trust-dependent act, so a change
  after that point affects the next act and terminal backends keep their existing behavior.
- Only a command carrying a trust option needs a spawn-time consent check
  (`a_command_without_a_launch_option_starts_whatever_the_consent_record_holds`).
- A grant must still match the provider, workspace, option and any settings file named by the
  plan (`the_spawn_fence_asks_about_the_commands_own_launch_option`).
- Inherited consent never authorizes a dialog key
  (`applied_or_inherited_consent_takes_no_key`).
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
  guarded dialog answer that a fresh `ask` can. Resolution may update shared consent before
  Session creation. A failed assessment leaves the reopen source available. Creation records
  the assessment before binding the Session to its source, and a failure to record it
  releases the reopen reservation.

