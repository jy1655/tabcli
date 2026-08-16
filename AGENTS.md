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
  including WSL 2, through its session messaging facilities. Prefer that official path
  when all Claude availability gates pass.
- Claude Code does not currently provide cross-session messaging on native Windows.
  Keep the Windows terminal-input path as a Claude-specific fallback with equivalent
  addressed-session semantics until Claude adds native support.
- Do not emulate Claude result correlation with a generic message hash. Identical valid
  responses can occur in separate turns; correlation must use Claude-owned identity or a
  provider-specific protocol with an explicit replacement boundary.

## Change and Verification Rules

- For provider behavior changes, add or update tests in that provider's adapter and prove
  that unrelated provider adapters do not need modification.
- For concurrency or lifecycle changes, reproduce the race or partial transition with a
  deterministic test before patching it.
- Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check`,
  and `git diff --check` before claiming completion.
- Tests requiring authenticated CLIs and visible terminal surfaces remain manual live
  tests; do not represent ignored live tests as runtime verification.
