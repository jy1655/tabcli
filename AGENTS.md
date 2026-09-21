# Agent Bridge Project Instructions

## Product Direction

Agent Bridge exists to make first-party Codex, Claude, Agy, and Pi sessions simple to
launch, observe, continue, and close without replacing the capabilities those CLIs own.

## Provider-Native First

- Prefer each provider's official session messaging, follow-up input, result identity,
  lifecycle, permission, and close mechanisms whenever the installed version, platform,
  provider backend, and configuration support them.
- Do not introduce one shared transport merely because multiple providers currently need
  similar fallback behavior. Provider-specific integration work is intentional product
  work, not duplication to eliminate.
- Keep shared orchestration limited to provider-neutral session state and lifecycle
  contracts. Provider payload schemas and transport selection belong in
  `src/native/provider/<provider>.rs`.
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

## Current Claude Boundary

- Claude Code v2.1.224+ provides official cross-session messaging on macOS and Linux,
  including WSL 2. Native Windows support is official from v2.1.234 and uses a
  per-session named pipe. Prefer the official path only when every runtime availability
  gate passes.
- Use the official `ListAgents` and `SendMessage` path on every supported Agent Bridge
  platform. Do not restore the removed native-Windows `--print --resume` supervisor or
  silently fall back to terminal input when official messaging is unavailable.
- Do not emulate Claude result correlation with a generic message hash. Identical valid
  responses can occur in separate turns; correlation must use Claude-owned identity or a
  provider-specific protocol with an explicit replacement boundary.
- Keep the follow-up payload out of the messenger model. The provider can stop or refuse
  a model response that carries arbitrary prompt text, which truncates the payload or
  skips `SendMessage` (observed 2026-09-18 with Claude Code 2.1.276-2.1.278). The
  messenger presents only a per-request reference, the `PreToolUse` guard supplies the
  addressed payload through the official `updatedInput`, and delivery counts only when
  the `PostToolUse` report of the executed input matches that payload.
- `SendMessage` is not permission-gated, even in `dontAsk` mode, and Claude discards the
  output of a hook that outlives its timeout and then runs the call unguarded. Neither a
  missing guard approval, the guard's own decision, nor an error result proves that
  nothing was sent: retry only when the guard approved nothing, Claude reported no
  executed call, and Claude itself reported every `SendMessage` call in the stream as
  blocked before it ran (denied with the guard's reason, or stopped by the provider).
- Give every request its own messenger files. The target can complete a delivered turn,
  and the next `tell` can start, while the previous sender is still settling.

## Change and Verification Rules

- For provider behavior changes, add or update tests in that provider's adapter and prove
  that unrelated provider adapters do not need modification.
- For concurrency or lifecycle changes, reproduce the race or partial transition with a
  deterministic test before patching it.
- Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check`,
  and `git diff --check` before claiming completion.
- Tests requiring authenticated CLIs and visible terminal surfaces remain manual live
  tests; do not represent ignored live tests as runtime verification.
- For Release workflow or release packaging changes, run the rehearsal
  (`gh workflow run release.yml --ref <branch> -f tag=<existing tag>`) and read its
  result before relying on a tag push. Only a tag push publishes, a published release is
  immutable, and `actions/checkout` rewrites the local tag ref to the commit, so tag
  properties are verified through the GitHub API rather than local refs.
