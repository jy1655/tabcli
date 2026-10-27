# macOS workspace consent and iTerm2 window lookup: 2026-10-27

This records the remaining human-origin checks for #28 and the iTerm2 lookup failure found
while performing them. The candidate changes only the iTerm2 terminal adapter; provider trust
stores, consent policy, provider transports, and permission flags are unchanged.

## Build and environment

- Base: `39f5727a2af6032592ab39f53dd7ae6e0ad65a1c` (v0.4.27).
- Installed baseline: `tabcli 0.4.27`, SHA-256
  `ce8167d6d8c6aa4840a6729b2a8bd7573e0f13e24445ba8777a1170fbfaab19e`.
- Local debug candidate, built with the iTerm2 change and regression test in this revision:
  SHA-256 `581f649ec3e3a910e10611abac7703e268d2a8676ed494fecca3de7ae2e0a3bf`.
  It also reports 0.4.27; the hash, not the version string, distinguishes it.
- macOS 26.5.2 (25F84), Apple Silicon; iTerm2 3.7.6, Terminal.app 2.15.
- Codex 0.164.0, Claude Code 2.1.311, Agy 1.4.7, Pi 1.4.0.
- Existing authenticated provider installations; no credential copies. All test launches
  omitted `--yolo`. The user's global Claude `permissions.defaultMode` was
  `bypassPermissions`, so a task-local executable wrapper prefixed the official
  `--permission-mode default` flag. The global setting was not edited. The observed Claude
  surface showed manual mode. External Codex used `-a on-request -s workspace-write`.
- Initial probes used the ordinary Bridge root. Subsequent probes used a separate private
  Bridge root with `macos-open-mode new-window`; provider homes and trust stores stayed real.
  Each fixture was a separate Git root. Prompts requested marker-only answers and no tools.

## Human-origin evidence

Three exact, previously untrusted directories were created. Before launching, exact-key reads
found no Codex, Claude or Agy approval for any fixture, and no Pi trust store. The operator
personally approved two directories in independently launched Codex TUIs and one directory in
Bridge-managed Agy (`session-6malSH`), and confirmed each action in the parent conversation.
Post-approval reads found exactly those new source decisions. The harness never wrote a
provider trust store or synthesized a Bridge consent record.

The Agy source's argument-delivered first request failed with a provider quota error before
the human approval. Its trust screen remained visible. The later manual approval, not that
failed result or a startup receipt, supplied the evidence reused below.

### Baseline approval transfer

| Human source | Target and terminal | Session | Observed scope |
| --- | --- | --- | --- |
| External Codex | Claude / Terminal.app | `session-GcDjc3` | Exact dialog answered; Claude saved trust; `provider-workspace-dialog`; initial/follow-up/close passed |
| External Codex | Agy / Terminal.app | `session-wtDFTg` | Exact dialog answered; Agy saved trust; `provider-workspace-dialog`; initial/follow-up/close passed |
| External Codex, second directory | Claude / Terminal.app | `session-kvbBAL` | Fresh target trust; initial/follow-up/close passed |
| Managed Agy | Codex / Terminal.app | `session-DLsQJI` | Source `agy`, `codex-config-override`; initial/follow-up/close passed; no persisted Codex trust entry |
| Managed Agy | Claude / iTerm2 | `session-fiChdO`, then `session-hvNcKN` | First session saved Claude trust through the guarded dialog but its messenger exhausted the remaining launch deadline; after inspection and explicit close, a distinct new request completed initial/follow-up/close without another manual approval |
| Managed Agy | Pi / Terminal.app | `session-43IUcF` | Source `agy`, `pi-approve-once`; initial/follow-up/close completed; see the stronger resource control below |

The first Claude/Agy 80-column screens wrapped the long fixture paths. These layouts did not
match the existing exact-path recognizers. Widening the task surface allowed recognition;
this is not evidence that wrapped paths are supported. In one Agy probe (`session-SUloBg`),
the argument-delivered request completed behind the dialog before resizing. The retired
claim correctly prevented a subsequent trust response; its success result is not a successful
approval transfer. The candidate Agy row below used a fresh launch and an early resize.

### Candidate iTerm2 checks

| Target | Session | Consent and result |
| --- | --- | --- |
| Agy | `session-9BMKgi` | External Codex source, previously untrusted Agy target, `provider-workspace-dialog`, provider-saved trust, initial/follow-up/close passed |
| Claude | `session-D5j2Kn` | Verified Codex source, Claude trust already saved by the Terminal.app probe; initial/follow-up/close passed; this is not a fresh Claude dialog check |
| Codex | `session-gfdSte` | `codex-config-override`; initial/follow-up/close passed |
| Pi | `session-uNI5io` | `pi-approve-once` with a real project resource requiring trust; initial/follow-up/close completed |

Initial answers were the requested exact markers. Pi appended a period to both recorded
follow-up answers (`session-43IUcF`, `session-uNI5io`); they prove completed follow-up delivery,
not exact-text compliance. The preserved answers are not normalized to make that check pass.

Pi's installed 1.4.0 security documentation says context files such as `AGENTS.md` load even
without project trust. The early Pi runs had only such context files and therefore did not
prove bypass of an actual project-resource gate. A benign `.pi/APPEND_SYSTEM.md` was added for
the candidate Pi run and the following negative control. No executable extension was added.

## Negative and automated consent checks

- Unapproved Claude (`session-rfJqoe`): the exact workspace dialog remained at its default
  No selection. Consent was `unverified`, `applied` absent, and readiness expired without
  sending the initial message. The session was closed.
- Before either human source approval, the external Codex and managed Agy trust screens
  remained visible. No automation answered those first approvals.
- Revoked managed-source consent (`session-flrP0q`): the public `consent revoke` command was
  applied only to the task directory in the private Bridge root. Pi displayed its real
  project-resource trust dialog, the request remained pending, and the assessment was
  `revoked` with no applied marker. No approval key was sent; explicit close ended the probe.
  Provider-owned trust was unchanged. The task root retains the revoked record as evidence.
- Exact source identity, parent/child/sibling paths, symlink aliases, replaced directories,
  unsafe stores, source removal, revocation races, and permission-dialog rejection have
  deterministic component coverage. These were executed, not claimed as new live scenarios.
- Unsupported Claude/Agy dialog response on Ghostty, WezTerm and Warp stays unsupported;
  these hosts were not treated as passing approval-sharing targets.

The [2026-10-19 Windows record](2026-10-19-windows-reopen-and-consent.md#claude-workspace-consent-28-258)
provides the separate human-origin Windows evidence. This Mac run does not repeat it.

## Reproduced iTerm2 failure and correction

The baseline returned AppleScript `Invalid index (-1719)` while reading a trust screen
(`session-zYtkDp`, `session-MeiuSM`) and while proving a Pi follow-up surface
(`session-PEA3wp`). Other tasks were creating, closing and selecting windows. The failed
follow-up had no new request address; no uncertain prompt was resent.

The cause was the unevaluated `windows` collection. Its nested object specifiers retained an
ordinal window address such as `item 2 of every tab of item 1 of every window`. A change in
front-to-back order could make that expression read a different window with fewer tabs.
`get windows` returns window-ID specifiers instead. The five session traversals now obtain
that list once before reading tabs: ownership, presence, screen/guarded input, paste, and close.
They keep their exact session UUID, ownership, lifecycle and screen checks. This does not freeze
tabs, panes or the renderer, and supplies no new authority if a target closes or changes.

A deterministic AppleScript model runs the actual ownership script with window ordering changed
at the second tab. The test failed before the fix with an invalid second-tab lookup and passed
afterward. A separate live comparison created two task-owned windows (two tabs in one, one in
the other), then selected the other window at the same point in the baseline/candidate scripts.
The baseline failed with `Invalid index (-1719)`; the candidate returned the expected
`/dev/ttys022`. All three temporary sleep sessions were closed individually. This controlled
comparison changed only the test-owned window order and did not read user terminal contents.

## Verification and boundaries

- Baseline full serial suite: 1,532 passed, 0 failed, 6 ignored, 18 suites.
- Candidate `cargo test --all-targets --all-features -- --test-threads=1`:
  1,533 passed, 0 failed, 6 ignored, 18 suites. The ignored tests are not runtime evidence.
- Candidate host Clippy, Linux/Windows cross-target Clippy, format and diff checks passed.
  Language lint passed for all 356 checked text files, including this report. Cross-target
  Clippy checks compilation, not native runtime behavior.
- The candidate was run by explicit path. The installed binary and plugins were not replaced.
- All 18 task-created Bridge sessions returned `closed: true, ok: true`. The two external
  source Codex sessions were closed by their exact iTerm2 UUIDs and then observed absent.
  Unrelated sessions were preserved.
- No Windows/Linux native runtime is claimed for this candidate. The change is macOS-only.
- Long wrapped paths remain outside the exact-dialog recognizer; widening a task surface was
  an explicit test setup step. An ordinary permission dialog, locked screen, or arbitrary
  simultaneous typing was not manufactured by these live checks.
- Failed baseline attempts, the Agy quota error, the Claude messenger timeout, and Pi's
  punctuation differences remain separate from successful rows. No failed or pending request
  was silently reclassified as a pass.

Private local evidence is retained in the task's `2026-10-27-issue-28` directory outside the
checkout: command/response JSON, source trust snapshots (exact keys only, no credentials),
per-request results, inspection/timeline/close responses, screen text, test logs, binary
identities, and the controlled iTerm2 reproduction. Provider transcripts and trust stores are
not copied into this repository.
