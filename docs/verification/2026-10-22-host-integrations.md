# Host integration verification (#285)

Performed 2026-10-22–23, native Windows, in the isolated `feat/285-host-integrations` worktree.
The invoking host and the target provider are different roles. This record covers the
instruction packages; it does not change provider transports or claim Linux support.
The implementation remains unreleased: v0.4.23 was already published without integrations.

## Environment and installation

- Windows with PowerShell 7 and Windows Terminal; workspace `D:/Dev/agent-bridge`, already
  trusted through the providers' own stores. No new workspace trust was granted.
- Installed engine `tabcli 0.4.22`; package version from candidate `Cargo.toml`: `0.4.23`.
  This verifies that the plugin and CLI versions need not match.
- Hosts/targets installed: codex-cli 0.163.0, Claude Code 2.1.308, Agy 1.4.5, Pi 1.4.0.
- Model and effort defaults were retained. The package itself sets no bypass flags.
- Every host's local installation was exercised in its own temporary configuration directory.
  Codex, Agy and Pi were additionally installed through their official commands in the normal
  user configuration for live discovery. Claude loaded the actual installed cache through
  its official per-invocation `--plugin-dir`, leaving ordinary Claude settings unchanged.

All four archives were generated, verified against source before extraction, and installed
using native host commands from their extracted directory. Claude/Codex archives supply their
own marketplace root and `plugin/` child. Agy/Pi archives are direct package roots. No package
depends on the source checkout at runtime; a local-path installation still needs its selected
source directory to remain present where the host's installation mechanism requires it.

| Host | Observed install/update/remove behavior |
| --- | --- |
| Claude 2.1.308 | Local marketplace add, plugin install/list, archive install, plugin uninstall and marketplace remove succeeded in isolated configuration. `/agent-bridge:agent-bridge` loaded the installed skill. |
| Codex 0.163.0 | Marketplace add plus plugin add copied the package to its versioned cache. A fixture changing only the source release from 0.4.23 to 0.4.24 was selected by a second plugin add. Marketplace upgrade correctly refused the local source (it refreshes Git sources). Removing the plugin and catalog preserved `model_reasoning_effort = 'medium'`. `$agent-bridge` loaded the installed skill in a new exec. |
| Agy 1.4.5 | Local install copied files to `%USERPROFILE%/.gemini/config/plugins/agent-bridge`. Installing the newer fixture replaced `integration.json` and retained disabled state. Uninstall then install enabled it again. Archive installation and removal succeeded. `/agent-bridge` loaded the skill in print mode. |
| Pi 1.4.0 | Local install recorded a path and loaded it in place. Archive installation, removal of the former source, listing the new source and removal all succeeded. `/skill:agent-bridge` loaded the skill. |

Claude's strict plugin/catalog validators passed. Agy's strict validator reported one skill
and no findings. The shared skill passed the skill-creator validator. The Agy schema documents
only `name`/`description`; package version is therefore recorded separately in `integration.json`.
No npm publication, public plugin-directory submission, or released tag containing these
packages exists yet. Tag-pinned installation is documented but is not represented as a live check.

## Host and provider round trips

These are authenticated model calls, separate from CI. Each successful row used the installed
skill, launched one target, read its exact request, sent a follow-up on that same session, read
the new exact request and explicitly closed only that target. The parent then re-read Bridge
results and `inspect` to confirm both completed records, `stored_state: closed` and a dead owner.

| Invoking host | Target | Initial request | Follow-up request | Session | Outcome |
| --- | --- | --- | --- | --- | --- |
| Claude Code | Codex | `request-1792680598617860400-25620-0` | `request-1792680627650975900-33344-0` | `session-4zf4UI` | Both exact markers completed; close confirmed |
| Codex | Claude Code | `request-1792680907918172000-33436-0` | `request-1792680937592071500-32072-0` | `session-Q2uEi0` | Both exact markers completed; close confirmed |
| Pi | Agy | `request-1792680472215012000-32612-0` | `request-1792680497548099400-31900-0` | `session-sCQzcW` | Both exact markers completed; close confirmed |
| Agy | Pi | `request-1792681180220666000-9536-0` | `request-1792681236716235300-32440-0` | `session-TgLqY6` | Both exact markers completed; close confirmed; native persistent shell exited |

Successful markers were `HOST_CLAUDE_TARGET_CODEX_INITIAL_285` / `...FOLLOWUP_285`,
`HOST_CODEX_TARGET_CLAUDE_INITIAL_285` / `...FOLLOWUP_285`, and
`HOST_PI_TARGET_AGY_INITIAL_285` / `...FOLLOWUP_285`, and
`HOST_AGY_PI_PERSISTENT_INITIAL_285` / `...FOLLOWUP_285` respectively.

Host permissions were explicit:

- Claude print mode used tool allow rules for `Bash(tabcli *)`, `Bash(command -v tabcli)`
  and `Read`. No target bypass was supplied. The first run could not find Codex: it existed
  only as a PowerShell function. Adding its actual npm launcher directory to that test
  process's PATH resolved the executable and the next new check passed.
- Codex's first `workspace-write` host was denied access to Bridge's private state before
  creation. The user then explicitly approved `--sandbox danger-full-access` for the test
  host. Another preflight correctly refused an obsolete Claude npm shim (2.1.226) that an
  overly broad PATH prepend selected. Restoring the original PATH selected native 2.1.308;
  a new check passed. Neither preflight created a session or sent target input.
- Pi ran with its existing defaults and encountered no tool permission denial.
- Agy print mode first auto-denied command permission. The user explicitly approved
  `--dangerously-skip-permissions` for the test host. The ensuing Pi target received initial
  input (`session-VNZrwM`, `request-1792680706256123300-4980-0`), but its owner was gone after
  the launch shell returned. That request was not resent and close was confirmed. The launch record reports
  `this desktop has no shell window` and a console-window fallback. This is observed failure,
  not proof of its process-termination cause. A new, distinct check with the approved bypass
  and explicit `--sandbox=false` used native `run_command` with `IsDaemon: true` to start
  `pwsh -NoLogo -NoProfile`, and `manage_task` (`send_input`/`status`) on the returned task.
  It read the initial completed JSON before writing the follow-up prompt. The same shell
  stayed alive through both results and close, then exited with code 0; Agy reported no
  remaining background tasks. Both Bridge results were independently re-read by the parent.
  This establishes the working native host procedure, not the internal cause of the earlier
  termination. No bypass was added to the Pi target.

These failures demonstrate that installing an instruction package does not grant shell,
state-directory or desktop access. They also exercise missing executable, incompatible CLI
and shell-restriction guidance without resending uncertain input. Runtime timeout and other
failure cases remain distinct from the packaging tests; no synthetic test is called a live check.

## Automated verification and review

- Local Windows: `cargo test --all-targets --all-features -- --test-threads=1`,
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check`, `git diff --check` passed.
- Python package suite: generation drift, release-version changes, relocation/reproducibility,
  manifest escape paths, unsupported components, extra files, invalid checksums, altered bytes,
  duplicate/traversal/link ZIP entries and exact release asset sets are tested.
- The Python suite has 12 tests: all run on Linux; macOS and Windows skip the Ubuntu release
  job's GNU Bash verifier. Windows-built checksums additionally passed Git Bash `sha256sum -c`.
- CI and nonpublishing Release rehearsal run URLs and final conclusions are recorded in
  [PR #288](https://github.com/jy1655/tabcli/pull/288). The rehearsal uses existing tag v0.4.23,
  which has no integrations: its release jobs verify the historical four CLI assets, while
  `integration-rehearsal` checks the candidate source. The exact combined twelve-asset Bash
  verifier is tested with fixtures on Ubuntu; actual publication of that new asset set remains
  for the next release. No tag or release was created for this verification.
- Claude Code collaborated on documentation and independently reviewed the package scripts
  and workflows. Its Windows checksum-newline and Python portability findings were fixed;
  the final review of `1474ea9` reported no blocking defects. Its four requests completed in
  `session-GmxlAp`, which was explicitly closed with no open children and a dead owner.
- Task-installed integrations in the normal Codex, Agy and Pi profiles were removed through
  their host CLIs. The remaining Codex catalogs were preserved, Agy listed no plugins and Pi
  listed no packages. The empty Agy import manifest created by this check was removed only
  after confirming it contained no imports. Evidence and isolated test profiles remain outside
  the checkout; no engine binary, workspace trust or global permission default was changed.
- All macOS live host/provider combinations and Windows combinations outside the table are
  unverified. CI's macOS package checks do not substitute for authenticated model calls.

Task-local raw command/model logs are outside the checkout under
`D:/Dev/agent-bridge-worktrees/issue-285-evidence/`; they are not packaged or published.
