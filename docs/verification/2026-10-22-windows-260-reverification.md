# Windows re-verification of Claude native completion

The first re-verification at `fa50fb8` **failed** the background-continuation case and
passed the prospective legacy-refusal recovery from #268. The owner then authorized
the correction below in [PR #283](https://github.com/jy1655/tabcli/pull/283), separate
from the unchanged original [PR #260](https://github.com/jy1655/tabcli/pull/260).
The earlier observations remain below; they are not evidence for the corrected binary.

## Correction: retain unmarked interim completion

The original exact-frame binding remains immutable. A normal nonempty bound answer
without the exact suffix marker becomes a private candidate and retains the claim.
A marked normal answer from a different main-loop turn can settle only with that
candidate, the exact live claim/pending request/binding and the same conversation.
The selected continuation record preserves both official turn ids before core
publication; another continuation cannot replace it after interrupted publication.
The Result keeps the original Bridge correlation id, so the synchronous caller can
finish, and uses `claude_mod_marked_continuation` provenance.

Bound abort, error, refusal and empty answer retain their failure behavior. An
unbound failure, malformed outcome, subagent, missing/damaged candidate, different
conversation, wrong/infix marker or oversized signal cannot settle the request.
The latter has no answer marker after JS reduces its oversized wire. Duplicate
helper delivery can repeat the same selection and yields at most one Result.

This is a provider-specific marker fallback, **not provider-owned parentage or
general request finality**. A same-conversation unrelated turn repeating the exact
marker can match; an interim carrying it can still finish early. An unmarked genuine
final can hold the claim indefinitely after caller timeout. `inspect` exposes the
current candidate under its existing reader lock and performs no retry, repair or
publication. Inspect the provider surface before closing or diagnosing the session.

The installed Claude 2.1.308 generated its actual `claude-code/index.d.ts` in the
owned production session's `.claude-plugin/types/`. SHA-256:
`3c29d70e7f113f2409db12c3db20dd8385296c46aaf26984d00e5ecacd37c2c0`.
Its `TurnCompleteFields`/`TurnStartInput` (lines 14168–14300) identify one turn and
loop, with no continuation-parent or addressed whole-request-final field.
`PromptOrigin` names task notifications, but `PromptSubmitInput.turnId` (9523–9530)
identifies an already running turn and is absent for idle submissions; it supplies
no parent relationship for the observed next turn. This check does not establish
that every other upstream facility lacks such a capability. Replace the fallback
when an addressed request-final signal or provider-owned continuation relation is
verified. Claude identifies generated declarations as authoritative for the loaded
build in its [official mods guide](https://claude.dev/blog/getting-started-with-claude-code-mods/).

Before implementation, the deterministic two-turn replay failed because the interim
had already been published. After correction it passes, including immutable binding,
exact original Bridge wait identity and duplicate publication checks. Provider unit
tests and the real JS-to-native-hook round trip also cover damaged selection,
claim/close fencing, timeout retention, unrelated outcomes and read-only inspection.
The final full-suite, native runtime and CI results are recorded below when complete.

## Source and environment

Native Windows, 2026-10-22, Claude Code **2.1.308**. The background probes' transcripts
report model **claude-opus-5-5**. Every run used the ordinary Bridge state root,
the already trusted `D:/Dev/agent-bridge` workspace and `--terminal windows-console`.
No run used `--yolo`, changed provider trust/permission settings, or answered a tool
approval. The process PATH appended npm to preserve native Claude's precedence.

| Build | Source and purpose | Executable SHA-256 |
| --- | --- | --- |
| Published baseline | Installed 0.4.22, legacy Stop completion | `63eb931664cc57cd92b3dce38864af85c221ab4e0df2628a2821b18ab3009c29` |
| Production candidate | `fa50fb8`, merge of unchanged #260 head `dfba55a` and main `6d8f54e`; reports 0.4.23 | `8196339e49b214bc67d37f839db1f6ce2567efec96dcc611c14c2320f312091a` |
| Diagnostic candidate | Same merge plus a temporary exact hook-stdin recorder; reports 0.4.23 | `4e3061eeaa89db172005564865274b0b47e0c632e0978c37e2a7fa27b4bda236` |

Both candidates were built with `cargo build --release --locked`. The diagnostic
build records `payload_text` before JSON decoding in `native/hook.rs`; its ignored
recording error cannot change the hook outcome. This probe is separate from the
decisive unmodified production comparison and is not included in the PR's source.
The installed executable remains 0.4.22.

## Background continuation: regression reproduced

The same prompt requested one harmless Bash call with `run_in_background: true`:

```sh
sleep 12; printf 'RECHECK260_BACKGROUND_FINAL\n'
```

It requested an unmarked `RECHECK260_INTERIM` after launch, then
`RECHECK260_BACKGROUND_FINAL` with the original delegation marker when the task
notification arrived, without another tool or file changes. Both production runs
followed that sequence, confirmed by their Claude transcripts.

| Build | Initial Bridge result | Result after the later final answer |
| --- | --- | --- |
| Baseline 0.4.22 | `RECHECK260_BACKGROUND_FINAL` | Same final answer |
| Production candidate | `RECHECK260_INTERIM`, completed | Still the interim answer |
| Diagnostic candidate | `RECHECK260_INTERIM`, completed | Still the interim answer |

Production candidate transcript times (KST): Bash launch at **22:35:48.453**,
unmarked interim at **22:35:50.735**, task notification at **22:36:01.464**, and
marked final at **22:36:02.734**. The result was published at 22:35:50.937 with
`claude_mod_turn_complete` provenance and was not replaced by the final answer.

The diagnostic build retained five exact stdin payloads: session.start, first
turn.start/turn.complete, then second turn.start/turn.complete. Both completions
named conversation `c00d8059-fee1-401b-84e0-02b17e628336`, reason `answer`, and
`isAborted: false`:

- Bound turn `eded8a6e-8668-4094-89ab-a8a5b573d74d` completed with the unmarked interim.
- Later turn `cd611302-2999-465f-bcac-2179e90eeda0` completed with the final text and
  the exact original request marker.

The immutable binding names only the first turn. `completion::outcome` accepts its
answer without requiring the marker, so it settles and releases the claim before
the later turn. This reproduces the previously reported failure on the newer
installed provider. It does not establish a provider-owned parent relationship for
the later turn. The subsequent correction was separately authorized by the owner.

## Fresh legacy refusal and reopen: pass

1. Installed 0.4.22 launched a fresh legacy session and returned `RECHECK260_LEGACY_OK`.
2. The production candidate refused its new tell before delivery with the native
   completion close/reopen guidance.
3. The receipt became **failed**, not unresolved. Its event has
   `provenance: delivery_not_sent`, no invented provider identity, and the refusal
   text. `result` agreed; timeline classified it as a delivery refusal.
4. The candidate closed the source and reopened it successfully on native completion.
   `RECHECK260_REOPEN_OK` and `RECHECK260_FOLLOW_UP_OK` completed with
   `claude_mod_turn_complete` provenance in the source's same provider conversation.

This verifies #268 prospectively for a new refusal settled by the new binary.
It does not repair or reinterpret historical unresolved receipts.

## Ordinary readiness and automated checks

The production candidate's ordinary `self-test claude` passed all five steps in
32.300 seconds: ask, initial result, tell, follow-up result and cleanup. Both results
have native completion provenance. Thus module readiness and delivery worked in this
installed configuration without a bypass flag; this is not a universal plugin-policy claim.

At `fa50fb8`, the native full serial suite passed **1,222 tests, 0 failed, 5 ignored**
across 17 suites. Native all-target/all-feature Clippy with warnings denied, formatting,
diff checks and locked release build passed. Native CI is tracked on PR #283; ignored
authenticated tests and cross-platform compilation are not additional live evidence.

## Cleanup and limits

All six owned sessions were explicitly closed and their status records checked:
`session-RzLwpE` (self-test), `session-hztULJ` and `session-amVid6` (legacy/reopen),
`session-mNJqmp` (baseline background), `session-yezP2S` (production background),
and `session-uAz5so` (diagnostic background). No unrelated session was closed and no
possibly delivered prompt was resent.

This round did not repeat the configured Stop-blocking hook, consent transfer,
steering, `/clear`, worker respawn, interruption, oversized completion or macOS
provider runs. Earlier #260 evidence for those cases remains dated separately.
These limits describe the initial re-verification round. The later correction above
does change completion policy; it does not change the installed executable.

Private evidence: `.git/260-live/` (JSON, stderr and exact diagnostic wires),
`.git/260-self-test.json`, `.git/260-*.log`, and the task-owned Claude transcript
files. The scripts `.git/260-legacy-reopen.ps1` and `.git/260-background.ps1` bound
commands, recorded failures, and closed only the sessions they created.
