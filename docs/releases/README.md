# Release notes

Choose your installed version below to check its changes and recorded test coverage. Dates are
from the local annotated tags. Curated notes start at 0.0.3; earlier tags have no notes here.
Older notes retain their own requirements and limitations; use the
[providers](../providers.md) and [terminals](../terminals.md) pages for current behavior.
Each note separates live checks from automated tests.

- [0.4.19](0.4.19.md) (2026-10-19): Recovers a Windows Claude reopen reservation after a proven launch failure (#248, #257); transfers genuine exact-workspace consent through Claude's Windows trust dialog under yolo with one guarded response (#28, #258)
- [0.4.18](0.4.18.md) (2026-10-18): Replaces the timed Windows console paste of a Codex initial prompt for injected launches with an evidence-based protocol: body and claim marker written separately, the rendered marker as completion evidence, one guarded Return under the lifecycle lock, Sent on the claim binding, structural dialog and picker recognition with documented refusals (#124, #255)
- [0.4.17](0.4.17.md) (2026-10-18): Moves Codex completion onto Codex 0.163.0's lifecycle hooks and the rollout's terminal record, which closes the stranded Windows completion above the command-line limit (#127, #250), with a Windows composer gate for the hook-review picker, `doctor` lifecycle evidence and `--probe` hook-trust comparison, and `settings codex-lifecycle-hooks`; refuses a Windows queue follow-up whose command line would reach the process limit before anything is sent (#251, #252)
- [0.4.16](0.4.16.md) (2026-10-17): Writes a Windows console surrogate pair as two key-down events so an emoji reaches Codex (#147); drops the Claude Code Codex plugin's caller-session names from every launch (#149); assesses workspace consent for a reopened Claude session as `ask` does, during resolution and before anything is created (#202); makes the doctor test's Agy store fixture private so the serial suite passes beside the Codex sandbox (#240)
- [0.4.15](0.4.15.md) (2026-10-17): Correlates a Codex turn steered by a message typed into the TUI (#65); compares Windows environment names as Windows does and unbinds the console helper test from PowerShell startup (#148, #180); documents the Windows notify payload limit with the doctor check `codex_notify_payload_budget` and keeps the framed prompt's size in the Codex pending-turn record (#127); records the usage Pi reports beside each request's result (#55, Pi)
- [0.4.14](0.4.14.md) (2026-10-17): `close-session --with-started-from` closes, one level deep and within the one `--timeout-secs` budget, each open session whose recorded origin is the closed session, through that session's own close, re-verifying its records first; each target is reported closed, skipped or failed and a failed target makes the exit non-zero after the full report (#219, part 3 of #216). Without the flag nothing changes.
- [0.4.13](0.4.13.md) (2026-10-17): Closes the seventh architecture review (#221 to #231): judgments decided once in the Observation (`inspect` no longer attests another session's owner record, `wait` ends only for a verified-gone owner, an undecided attestation is `owner_unverified`), one decoder per core record (strict UTF-8, `*_unix_ms` range, regular files only for `status.json` and `closed.json`), an empty `CLAUDE_CONFIG_DIR` unset for the trust store; close authority, launch, the provider launch command, the Turn run, provider homes, terminal registration, the native command modules, the Claude messenger and the clock seam restructured with no behaviour change.
- [0.4.12](0.4.12.md) (2026-10-16): Answers #216. `settings codex-memory-workspace refuse|allow` (default refuse) makes `ask`, `reopen` and `self-test` refuse a launch in the Codex memory workspace before any record or surface exists (#217); `status`, `sessions`, `inspect` and `doctor` report a session's origin and whether it ended, and `close-session` lists the open sessions started from the closed session (#218).
- [0.4.11](0.4.11.md) (2026-10-16): Closes the sixth architecture review. With `--json`, `ask` and `tell` print a JSON error envelope when they fail before a request address exists (#200); session start is one operation (#203); reopen asks each provider once (#201); each adapter owns its live-owner pre-close (#197) and close runs through a named attestation seam (#198, #199); the Codex and Pi adapters are module directories (#207, #209); the Unix owner and mode questions, a provider's version text, a capped command output and the session's recorded end each have one home (#208, #211, #214, #205); host commands are declared once (#206); the Pi extension's core record reads are a declared contract (#195).
- [0.4.10](0.4.10.md) (2026-10-15): One attestation of the recorded owner and of every recorded incarnation (`process_identity`, `session::owner::attest_owner`; #190, #191): a reused pid is never absence, an inspection failure is never a death, a dead legacy Windows owner no longer blocks prune, an uninspectable live Windows owner stops repair, a verified Windows reuse is reported like on macOS; the launch wrapper's admission and exit settlement live in `session::launch` (#192); glossary gains Incarnation.
- [0.4.9](0.4.9.md) (2026-10-12): Starts the managed PowerShell root and the provider forwarder from the plain path on native Windows (module auto-loading works again inside managed sessions); closes the refactoring, anti-slop and test-hardcoding review (#151 to #163): tests split by subject, timeline renderer and Claude registry module, dead code and duplicate helpers removed, test values derived from their owners, deterministic timing tests, comment sweep.
- [0.4.8](0.4.8.md) (2026-10-12): A Codex Windows console paste keeps its line feeds; non-provider children on native Windows drop the caller's session markers; the README Install section is pinned to the Cargo version; the hand-off tests wait for the other thread.
- [0.4.7](0.4.7.md) (2026-10-12): `sessions` counts published Results; one state-root enumeration; the AppleScript adapters get an observable automation seam; the bounded command runner, the result-timeout diagnostic and the Windows console-control action are typed and placed. Closes architecture review 5.
- [0.4.6](0.4.6.md) (2026-10-12): Adapters declare the launch-trust, binding and terminal facts the shared modules branched on; each macOS adapter owns its operation set; the JSON envelope has one home. No output change.
- [0.4.5](0.4.5.md) (2026-10-12): Declares close evidence per terminal adapter, splits the Agy adapter into modules, and names the turn claim's launch phases; no behaviour change.
- [0.4.4](0.4.4.md) (2026-10-11): Waits for the Windows console input buffer before the submission Return and
  requires the empty Codex composer after a paste; one delivery module with adapter-declared Transports;
  the consistent session read behind the session seam.
- [0.4.3](0.4.3.md) (2026-10-11): Passes the session directory to the terminal adapters; adapters
  declare their input facts; refuses close where no terminal transport exists; one command registry.
- [0.4.2](0.4.2.md) (2026-10-11): Closes a session whose terminal application went away while its
  processes kept running; gives `close-session` one `--timeout-secs` budget; reports a busy WezTerm.
- [0.4.1](0.4.1.md) (2026-10-11): Keeps Claude Code session markers and Bridge's per-session
  variables out of the processes Bridge starts on macOS, including a terminal application it starts.
- [0.4.0](0.4.0.md) (2026-10-11): Adds `cancel` for Pi sessions; starts iTerm2 when it is not
  running; says what a terminal creation failure left behind; records per terminal why a guarded
  trust response is or is not possible.
- [0.3.0](0.3.0.md) (2026-10-11): Adds `status`, `wait`, and `hold`; renders `inspect` and doctor
  from one session observation; the doctor report states its scope.
- [0.2.6](0.2.6.md) (2026-10-07): Uses the provider Report path for completion tests, separates
  Request observation from JSON, and concentrates Reopen and Agy Result evidence rules.
- [0.2.5](0.2.5.md) (2026-10-07): Repairs dead owners despite completion recovery damage;
  removes unused delivery fallback and consolidates status writes and helper containment.
- [0.2.4](0.2.4.md) (2026-10-07): Consolidates launch waits and delivery settlement, preserves
  delivery failure reasons and residual-surface evidence, and localizes timeout diagnostics.
- [0.2.3](0.2.3.md) (2026-10-07): Keeps the record of a Warp launch that fails after its tab
  exists; reports in `doctor` and in a timed-out self-test whether Pi has credentials for a
  session's provider; explains the Codex line about the shared background server.
- [0.2.2](0.2.2.md) (2026-10-07): Keeps the surface of a Ghostty launch that fails after its tab
  exists so that a later close can close it; names a pending Agy tool approval in a timed-out
  self-test; says that a launch may need the screen unlocked.
- [0.2.1](0.2.1.md) (2026-10-07): Fixes a rare failure when closing a Warp session; the macOS
  permissions page and the release procedure are now in English.
- [0.2.0](0.2.0.md) (2026-10-06): Renames the command and package to `tabcli`; your sessions and
  settings are read as they are. Update scripts that call `agent-bridge`.
- [0.1.2](0.1.2.md) (2026-10-06): Reorganizes session records and terminal ownership;
  no migration or behavior change.
- [0.1.1](0.1.1.md) (2026-10-04): Codex follow-ups work without a shared daemon.
- [0.1.0](0.1.0.md) (2026-10-03): Adds macOS terminal selection and opening modes; restores Ghostty.
- [0.0.10](0.0.10.md) (2026-10-02): Fixes Codex startup and result correlation; adds request
  timelines, observed elapsed time, and self-test.
- [0.0.9](0.0.9.md) (2026-10-02): Shares workspace consent and opens Windows Terminal tabs.
- [0.0.8](0.0.8.md) (2026-09-28): Adds launch diagnostics and a Pi startup gate; native Windows
  launches are broken. Upgrade to 0.0.9 or later.
- [0.0.7](0.0.7.md) (2026-09-24): Adds result search, prompt context, and Windows Claude reopen.
- [0.0.6](0.0.6.md) (2026-09-23): Adds session inspection, results by request, and diagnostics.
- [0.0.5](0.0.5.md) (2026-09-21): Keeps Claude follow-up text out of the messenger model.
- [0.0.4](0.0.4.md) (2026-09-04): Addresses Codex follow-ups through its native queue.
- [0.0.3](0.0.3.md) (2026-08-28): Ships a native Windows archive with a SHA-256 checksum.

Published releases are immutable. Follow [the release procedure](../releasing.md) to publish a
release or verify its archive checksums and attestations.
