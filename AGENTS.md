# TAB Project Instructions

## Product Direction

TAB exists to make first-party Codex, Claude, Agy, and Pi sessions simple to
launch, observe, continue, and close without replacing the capabilities those CLIs own.

The installed skill carries the general delegation rule (owner's decision, 2026-11-05): a host
uses its own subagents for a child on its own provider, and TAB for another provider or
when the user asks for TAB. Per-user launch details (terminal, permission mode, model,
effort, session budget) stay in the user's own instructions, not in the skill. The rule rides
in the skill only: TAB does not launch the host session, so it has no system prompt to
extend, and an integration adds no hook or MCP server to carry it. Do not build a TAB path
that duplicates what a provider's own subagents do for a same-provider child.

Behavior is the same on every OS by default (owner's decision, 2026-11-26). Keep a platform
difference only where a platform fact makes it unavoidable, and state that fact where the
difference lives. A difference inherited from earlier code is not a reason by itself.

## Change and Verification Rules

- Public documentation and default plugin metadata use English. Run `python3 scripts/check_language.py`;
  translations and Unicode fixtures have explicit exceptions described in `docs/testing.md#language-lint`.
- For provider behavior changes, add or update tests in that provider's adapter and prove
  that unrelated provider adapters do not need modification.
- For concurrency or lifecycle changes, reproduce the race or partial transition with a
  deterministic test before patching it.
- Run `cargo test --all-targets --all-features -- --test-threads=1`,
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check`, and
  `git diff --check` before claiming completion. Use serial test-harness execution for
  local, CI, and release checks; retain concurrency created inside individual tests.
- Finish every release with local build cleanup after publication verification, installation
  (if requested), and all checks. Follow `docs/releasing.md#local-build-cleanup`: preserve
  evidence outside `target/`, ensure no build or live session depends on its binaries, then
  run `cargo --config 'build.build-dir="./target"' clean --target-dir ./target` from the
  checkout root. Clean only this checkout's build output, never other worktrees, installed
  binaries, session records, a separately configured build directory or global Cargo caches.
  Report reclaimed space, or the dependency or verification gap that deferred cleanup.
- Tests requiring authenticated CLIs and visible terminal surfaces remain manual live
  tests; do not represent ignored live tests as runtime verification.
- A claim about Windows behavior is not verified until its `cfg(windows)` test has run on the
  Windows CI runner. Cross-target clippy proves compilation only, and a review's source-level
  mock of Win32 can contradict the system (2026-10-15, #191: the mock had the identity query of
  an exited process succeed; Windows refuses it).
- For Release workflow or release packaging changes, run the rehearsal
  (`gh workflow run release.yml --ref <branch> -f tag=<existing tag>`) and read its
  result before relying on a tag push. Only a tag push publishes, and a published release
  is immutable.
- In the Release workflow, verify tag properties through the GitHub API: the default
  tag-push checkout rewrites the local tag ref to the commit, so only the remote still
  holds the tag object. Do not replace the API check with a second fetch: the checkout
  keeps no credentials, and the check must not depend on the repository's visibility.
  Only validation resolves the tag name; later jobs check out the validated commit SHA,
  and publication re-checks that the remote tag is still the validated tag object,
  because a tag can be moved while the run is in progress.
- Release scripts receive workflow expressions through `env`, never inline, so the policy
  tests can execute the exact scripts against fixtures.

## Provider-Native First

- Prefer each provider's official session messaging, follow-up input, result identity,
  lifecycle, permission, and close mechanisms whenever the installed version, platform,
  provider backend, and configuration support them.
- Do not introduce one shared transport merely because multiple providers currently need
  similar fallback behavior. Provider-specific integration work is intentional product
  work, not duplication to eliminate.
- Keep shared orchestration limited to provider-neutral session state and lifecycle
  contracts. Provider payload schemas and transport selection belong in
  `src/native/provider/<provider>/` (or a single `<provider>.rs` file).
- Every provider adapter must explicitly own its launch configuration, follow-up
  transport, completion identity, and any unsupported-platform fallback. Do not add a
  default adapter implementation that silently assigns the same behavior to all CLIs.

## Replaceable Fallbacks

- A fallback is allowed only where the first-party CLI lacks the required capability on
  the current platform, version, provider backend, or configuration.
- A fallback must preserve the semantics of the missing first-party feature, remain
  isolated inside that provider adapter, and document the upstream condition that makes
  it removable.
- When upstream support arrives, prefer deleting or replacing the provider fallback over
  extending the shared layer. The expected long-term direction is less bridge code as
  first-party support improves.
- Never claim a first-party path is active from version checks alone. Verify all relevant
  availability gates and keep the fallback explicit when the official feature is absent.

## Required task-specific instructions

Before editing or reviewing a path or behavior below, read the corresponding file in full.
These are project instructions, not optional background. Linked files are not assumed to
load automatically: Codex reads this entry, and Claude imports it through `CLAUDE.md`.
For shared lifecycle, launch, input, environment or record changes, read every affected
provider/surface file as well as the cross-cutting files. Follow behavioral dependencies,
not just the directory of the edited file. Read `CONTEXT.md` for the existing domain names.
Paths mentioned inside the boundary files are relative to the repository root.

| Read | When the work touches |
| --- | --- |
| [Current Claude Boundary](docs/agents/claude.md) | Claude launch, messaging, completion, questions or caller environment; `src/native/provider/claude.rs` and `src/native/provider/claude/`. |
| [Current Agy Boundary](docs/agents/agy.md) | Agy trust, input, receipts or failure monitoring; `src/native/provider/agy/`. |
| [Current Pi Boundary](docs/agents/pi.md) | Pi extension, input, pending turn, cancellation or usage; `src/native/provider/pi/` and `src/native/provider/pi_*` extension/fixture files. |
| [Current Codex Boundary](docs/agents/codex.md) | Codex hooks, input, completion, questions, memory admission or caller markers; `src/native/provider/codex/`. |
| [Current Linux Surface Boundary](docs/agents/linux.md) | Linux launch, input, observation, close, process identity or exec budgets; `src/native/terminal/linux/`. |
| [Current Windows Surface Boundary](docs/agents/windows.md) | Windows console/tab launch, focus, host, control or record publication; `src/native/terminal/windows/`, `src/native/console_control.rs` and `src/native/console_control/`. |
| [Current macOS Shell Startup Boundary](docs/agents/macos-shell.md) | macOS shell capture or launch host; `src/native/terminal/macos/` (especially iTerm2/WezTerm). |
| [Current Herdr Surface Boundary](docs/agents/herdr.md) | Herdr socket or Windows pipe, launch, caller verification, input, screen or close; `src/native/terminal/herdr.rs`, `src/native/terminal/herdr/` and `src/native/terminal/windows/launch_host.rs`. |
| [Trust Store Boundary](docs/agents/trust.md) | Workspace consent, provider trust readers, launch dialogs or reopen admission; `src/native/consent/` and provider trust code. |
| [Observation and Self-Test Boundary](docs/agents/observation.md) | Queries, screen, timelines, delivery refusal, version notes or self-test; `src/native/query/`, `src/native/self_test/` and diagnostics. |
| [Current Attestation Boundary](docs/agents/attestation.md) | Owner/process observation, repair, wait, prune, close or reopen; process identity and ownership code on every platform. |

Keep universal rules and required checks here; update the owning boundary file for details.
Run `python3 scripts/check_agent_instructions.py` when changing these instructions or their
routing. It enforces the 32,768-byte ceiling, the Claude entry and the boundary links. Leave
room below the ceiling for ancestor/user instructions; a local larger Codex budget is not a
substitute for keeping this entry small.
