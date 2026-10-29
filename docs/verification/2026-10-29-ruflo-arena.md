# Ruflo-inspired diagnostics and comparison tools

Date: 2026-10-29. Base: `569598e1672730e0d4979592c18f11c059225931`.
This records local implementation and verification, not a release, installation,
performance improvement measurement, or authenticated coexistence check.

## Candidate selection

Four features each had two isolated implementations from the same base: A used a native
Codex Sol worker, B used Claude Code Opus through Agent Bridge. Implementation effort was
xhigh, with max for the request-correlation tests. Each pair received a fresh Astra review;
the parent independently read the complete diffs, new files, tests, documentation and
rationale, then compared scores. All eight candidates completed. The four task-owned Claude
sessions were closed after their exact results were retrieved.

Scores use five equally weighted criteria, each 0-4: useful scope, existing contracts and
privacy, evidence quality, maintainability, and bounded operational behavior. A contract or
privacy failure disqualifies a candidate regardless of its total.

| Feature | Parent A / B | Independent judge A / B | Selected base |
| --- | --- | --- | --- |
| Claude completion diagnostics | 15 / 19 | 13 / 18 | B |
| Inbound module coexistence regressions | 15 / 13 | 15 / 13 | A |
| Saved self-test comparison | 16 / 12 | 16 / 15 | A |
| Offline diagnostic sharing | 17 / 8 | 17 / 8 | A |

### Final changes and rejected alternatives

- **Completion diagnostics:** reuse the adapter's typed record decoder, bounded reads and
  category-only decoding errors. Mode/readiness and latest diagnostic are separate doctor
  checks. Adopt A's conservative `unknown` for valid historical readiness and independent
  damaged-record coverage. Add a malformed numeric-field canary test. A record without a
  request id or timestamp cannot establish which request was rejected or how recently.
- **Coexistence:** retain A's real messenger guard/receipt path and actual JavaScript module
  round trip through the candidate native-hook binary. Correct A's successor fixture to
  enter `Claimed` before delivery. Add B's explicit timeout cause, second-claim refusal and
  preservation assertions. Reject B's live probe: unlimited logging, ignored recorder
  failures and record-mtime-based hook-order inference do not establish bounded evidence
  or causality. No production delivery or completion policy changed.
- **Comparison:** retain A's fixed step vocabulary, whole-input bound and explicit
  environment/backend/configuration/cold-warm controls. Graft confirmed `not_run` treatment
  and cross-baseline duplicate-run refusal. Also fix integer conversion before parsing
  oversized tokens, and accept elapsed values only in the integer u128 range. Every failed
  attempt, cleanup outcome, input hash and sample pointer remains observable. Reject the
  broader manifest/variation API and dynamically expanding metric vocabulary.
- **Sharing:** retain A's stdin/stdout-only typed projection, strict complete version forms,
  omitted identifiers, and input/depth/node/array/output bounds. Graft fixed record-file
  families and question-observation incompleteness qualifiers; add current doctor categories.
  Reject B's aliases/file-output/rendering framework. Synthetic inputs reproduced B copying
  a numeric canary from arbitrary version text and throwing an exception for a long list of
  valid attention flags. The final tests cover both counterexamples.

The offline tools are documented in [self-test comparison](../self-test-comparison.md) and
[diagnostic sharing](../cli.md#offline-diagnostic-summary). The coexistence fixtures and
optional, unverified manual recipe are in [testing](../testing.md#claude-inbound-module-coexistence).

## Verification

Environment: macOS 26.5.2 arm64, rustc 1.99.1, Cargo 1.99.1, Node.js v24.12.0,
Python 3.12.15 and system Python 3.9.6. Rust tests ran serially with a target directory
dedicated to the integration worktree and `CARGO_INCREMENTAL=0`.

| Check | Result |
| --- | --- |
| `cargo test --all-targets --all-features -- --test-threads=1` | 1,551 passed, 0 failed, 6 ignored across 18 suites |
| `cargo clippy --all-targets --all-features -- -D warnings` | Passed |
| `cargo fmt -- --check` | Passed |
| `python3.12 -m unittest discover -s scripts -p 'test_*.py' -v` | 100 passed, 2 existing platform-specific skips |
| Focused comparator / formatter suites on Python 3.9 and 3.12 | 24 / 18 passed on each version |
| `scripts/check_language.py` and `git diff --check` | Passed |
| Candidate CLI public JSON through the final formatter | Provider doctor, closed-session doctor and timeline passed |

The Rust ignored entries are five manual live tests and one subprocess fixture invoked by
another deadline test. Python skips require Ubuntu's GNU Bash release job and the Windows
junction API. These gates do not claim those platforms or live sessions were exercised.

The actual local query check used a task-owned, closed Claude session. Both completion doctor
categories survived the sharing projection. Valid recorded readiness remained `unknown`.
The queried candidate binary SHA-256 was
`0b75d6bd1913c7dbeaa3802fd257a150af1099b08dc2c4236037c670d90c19a6`.
This verifies public-output integration, not current module readiness or delivery.

An early candidate run reused a shared Cargo target directory and executed a baseline CLI.
Its three doctor integration failures were a parent build-harness error. Both candidates were
rerun with isolated target directories; only those isolated results informed selection. The
integration gate also used its own target directory.

## Remaining evidence boundaries

Authenticated `session.receive` consume/rewrite behavior, actual plugin ordering, Windows
runtime, and fresh-session behavior with the changed binary remain **NOT-VERIFIED**.
The comparison tool enables measurements; no before/after performance improvement is claimed.
Sharing is a lossy projection of supplied JSON, not proof of authenticity or anonymity.
No provider settings, installed executable or global plugin artifacts were changed.
