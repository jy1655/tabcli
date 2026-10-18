# Claude native completion (#249): Windows owner verification

## Scope

The implementation owner ran these checks on 2026-10-19 with Claude Code 2.1.302, Opus,
native Windows and the managed `windows-console` terminal. Every authenticated Bridge launch
used `--yolo`. Bridge state and the probe configuration were isolated. All eight owned probe
sessions were closed. No unrelated session was closed.

The live evidence predates the fixture privacy rewrite, rebase onto main and review corrections.
The final live candidate was a debug build reporting tabcli 0.4.18. Its SHA-256 was
`195FB1D49CF2301BF8BDFDFE2A3797381AD880918A725F44242EED9BEDB4064B`.
These owner probes are not independent final parent/head verification.

## Live observations

| Check | Observation |
| --- | --- |
| Legacy baseline, ask and tell | Installed 0.4.18 stored `PROBE-DRAFT` at Stop. The later official completion carried `PROBE-FINAL`, but subsequent result queries still returned the draft. |
| Native completion, ask and tell | Bridge published `PROBE-FINAL` after the blocked draft Stop and the matching official completion. The final answer omitted the marker. Later queries still returned the final answer. |
| Protected peer-body tags | An earlier candidate retained the claim because Claude quoted protected tags before `turn.start`. The corrected candidate bound both requests with 1,200 quoted closing delimiters each. Each captured start was 53,435 UTF-8 bytes. |
| Correlation | Two distinct official turns in one conversation mapped to two distinct Bridge correlation ids. Core results retained the Bridge ids and native completion provenance. |
| Disabled hooks | `disableAllHooks: true` produced no module readiness or pending record. The initial delivery failed as not sent at the readiness deadline. |

The Stop blocker was a private diagnostic module's `classic.Stop` interceptor. It awaited
`next(e)`, blocked the first Stop once, then allowed the continuation. A launcher shim added
that diagnostic plugin. The shipping completion module intercepts no classic hooks. These
probes establish actual provider continuation, but do not verify a configured user Stop
command hook on an unmodified production launch.

## Deterministic evidence

After privacy sanitization and rebasing onto main `193e8e7`, both original regressions compiled and
failed for their original reasons. Native-mode Stop still published a draft before the fix,
and the official completion wire failed legacy parsing. The absent-mode legacy Stop guard
passed. The second red commit compiled and failed because the original matcher rejected
Claude's quoted peer body. Sanitization changed identifiers and removed unused capture fields,
without changing assertions.

The adapter tests exercise real claims, immutable bindings, publication and Bridge wait identity.
They cover exact framing, special characters, conflicting and duplicate events, claim replacement,
close fencing, pending cleanup ownership, readiness, version eligibility, legacy isolation,
outcomes and wire bounds. A controlled Node messenger drives `send_for_claim(Some(token))`
through the real pending writer, guard and receipt functions. It compares the stored expected
message with the controlled executed-input report, rather than formatting the expected value twice.

The shipping JavaScript module also runs against a controlled API and forwards its actual
JSON through the Rust `native-hook` stdin entry. Seven scenarios cover Unicode beyond the
Windows argv limit, exactly 4 MiB, one byte over the limit, helper failure before execution,
nonzero helper exit after publication, repeated helper failure and subagent exclusion.
Those controlled scenarios are not authenticated provider outcomes.

The correction pass ran these focused checks on native Windows at `88a45c8` on main `193e8e7`.
The later 0.4.19 rebase onto `7ce83b5` kept every patch identical. These are the earlier run's
results, not reruns after that rebase. Every test command used `--test-threads=1`.

| Command | Result |
| --- | --- |
| `cargo test --bin tabcli native::provider::claude:: -- --test-threads=1` | 66 passed |
| `cargo test --test claude_completion -- --test-threads=1` | 1 passed, covering seven scenarios |
| `cargo test --bin tabcli native::reopen:: -- --test-threads=1` | 56 passed |
| `cargo test --bin tabcli native::consent:: -- --test-threads=1` | 34 passed |
| `cargo clippy --all-targets --all-features -- -D warnings` | Passed |
| `cargo clippy --all-targets --all-features --target x86_64-unknown-linux-gnu -- -D warnings` | Passed, compilation only |
| `cargo clippy --all-targets --all-features --target x86_64-pc-windows-msvc -- -D warnings` | Passed, compilation only |
| `cargo clippy --all-targets --all-features --target aarch64-apple-darwin -- -D warnings` | Passed, compilation only |
| `cargo fmt --all -- --check` and `git diff --check` | Passed |

The full serial suite was not rerun in that correction pass.

## Unverified cases

The owner did not drive a configured user Stop command hook, ordinary manual turns, steering,
subagent work, delivery into an already-running manual turn, combined queued input, Esc,
API error, refusal, empty answer, `/clear`, worker respawn, close during a running turn, or
an in-flight legacy binary upgrade. Non-yolo plugin trust and policy behavior and macOS
runtime were not verified. A model-generated 3 MB prompt or answer was not attempted.

Running-turn injection without its own exact start, and combined queued input that differs
from the exact addressed frame, remain unbound by construction. This is a matching limitation,
not an observed provider queue result. Deterministic outcome tests and cross-target compilation
do not close the live gaps. CI and independent final actual-parent/head validation have not
run for this corrected candidate.
