# macOS Claude completion after a blocking Stop hook

Issue [#249](https://github.com/jy1655/tabcli/issues/249) was reproduced on macOS
with Claude Code **2.1.309** and iTerm2. On the baseline, both `ask` and `tell`
published `PROBE-DRAFT`, although a configured Stop hook continued the turn and
Claude's official `turn.complete` subsequently reported `PROBE-FINAL`.

The baseline source was PR #300 head `55e09abdc2b0e8d8f018821f85f9cbf811d4ed77`.
Its tree is identical to the merged main commit
`4c0db704d385e49972a017c9f668c910201ae4e7`, which is this change's base.
The baseline executable was built with `cargo build --locked` and copied before
the production changes. Its SHA-256 is
`b8a5a41c5f252126c36589dbf99d754a8f8b67ced5f90548040244df7f18ff2d`.

## Reproduction method

The committed [probe](../../scripts/verify_claude_stop_completion.py) uses a new
private Bridge state root under its output directory. It launches one task-owned
Claude session, performs an initial request and a follow-up, reads
each exact request's result, checks for one published event per request, and
inspects and closes that session. It requires macOS, iTerm2, an authenticated
Claude installation, and the usual workspace trust prerequisites.
It does not inherit the ordinary Bridge root's settings or consent records.
Claude's own configuration and authentication still apply. Cleanup discovers
owned sessions from the private root even if launch JSON never returns, skips
symlinked session directories, and writes subprocess output directly to files so
already-written output survives timeout or interruption. It never
enumerates or closes sessions in the user's ordinary Bridge root.

A private launcher in the probe's output directory adds an ordinary configured
Stop command hook and a diagnostic observer plugin to that launch only. It leaves
the Bridge-generated settings file and the user's settings unchanged. The hook
records each payload and blocks the first draft, requesting a final answer. The
observer records official main-loop start and completion identities. Both ask
and tell must show a draft Stop with `stop_hook_active: false`, a final Stop with
`stop_hook_active: true`, and a normal official completion for the same turn and
conversation. The Bridge result must match the expected baseline or fixed answer.

```sh
python3 scripts/verify_claude_stop_completion.py \
  --bridge /absolute/path/to/baseline-tabcli \
  --workspace /absolute/path/to/agent-bridge \
  --output /tmp/tabcli-249-before \
  --expect draft --yolo

python3 scripts/verify_claude_stop_completion.py \
  --bridge /absolute/path/to/candidate-tabcli \
  --workspace /absolute/path/to/agent-bridge \
  --output /tmp/tabcli-249-after \
  --expect final --yolo
```

Output directories must be new. `--yolo` is explicit in these controlled probes;
it does not establish behavior under ordinary permission policy. The probe adds
no provider trust acceptance or global settings change.

The reusable baseline run produced:

```text
ask: provider=PROBE-FINAL bridge=PROBE-DRAFT
tell: provider=PROBE-FINAL bridge=PROBE-DRAFT
PASS: ask and tell match expected draft; owned session closed
```

That baseline session was `session-TdbIJB`, provider conversation
`33b3baef-aaf4-4dc6-9c4c-4a093c7de698`. The exact requests and official turns were:

| Request | Bridge request ID | Official turn ID |
| --- | --- | --- |
| ask | `request-1792743811642798000-93944-0` | `b8897175-610b-44f9-a819-10115dc20f03` |
| tell | `request-1792743820607864000-95123-0` | `00756c67-98be-4c82-9332-4363cb4ffbc4` |

The deterministic regression was committed before the production fix in
`b31a93c`. Its unchanged-production failure was:

```text
fresh_macos_launch_ignores_draft_stop_until_native_completion ... FAILED
fresh macOS launch published a draft Stop before native completion
  left: 1
 right: 0
```

## Candidate macOS observations

The implementation candidate is **`7c695801884bced7cf401d2a8ca96c30a43d6fba`**,
built with `cargo build --locked`. Executable SHA-256:
`60037d20f0ff2f4540b4bad56eaa6bb6b911a55552390913cff2e6c17c577a1a`.
The following are authenticated local observations on Claude 2.1.309 and iTerm2.

| Check | Observation |
| --- | --- |
| Configured blocking Stop, `ask` | The draft and continued final Stops were observed. The addressed Bridge result was exactly `PROBE-FINAL`, with `claude_mod_turn_complete` provenance. |
| Configured blocking Stop, `tell` | The same sequence produced exactly `PROBE-FINAL`. `result --list` contained exactly two events, one for each probe request. |
| `disableAllHooks: true` | Initial delivery failed with the readiness-deadline reason and no message sent. No readiness, pending-turn or messenger record existed. The target surface was inspected before close. |
| Ordinary `self-test claude`, no `--yolo` or diagnostic plugin | All five steps passed in 48.578 seconds, including ask, tell, addressed results and cleanup. The original repository already had Claude's own exact workspace trust. |
| Background ask and tell | Each unmarked interim remained a candidate. Each marked final settled with the original Bridge correlation ID and a separately recorded official continuation. |
| Genuine unmarked final | The 25-second caller timed out with the request still pending and the claim retained. Inspection showed the candidate; no third success event was published. |

The configured-Stop candidate run used session `session-X1gAkt`, conversation
`f73e2455-8d35-463d-8d7d-287935181486`:

| Request | Bridge request ID | Bridge correlation ID |
| --- | --- | --- |
| ask | `request-1792744761178513000-2158-0` | `claude-turn-1792744765943957000-2158-1` |
| tell | `request-1792744796087286000-3960-0` | `claude-turn-1792744796102230000-3960-1` |

The probe printed:

```text
ask: provider=PROBE-FINAL bridge=PROBE-FINAL
tell: provider=PROBE-FINAL bridge=PROBE-FINAL
PASS: ask and tell match expected final; owned session closed
```

The first runs above used the ordinary Bridge state root. After the probe's
failure-cleanup review, both baseline and candidate were repeated using the
committed private-root implementation, with the same draft/final outcomes and
confirmed closure. A fault-injection test first reproduced skipped cleanup after
launch timeout. The five passing Python tests cover timeout, malformed JSON,
interruption, unrelated-session preservation and output preservation. A real
SIGINT probe first reproduced lost flushed output; writing subprocess streams
directly to task-owned files fixed that diagnostic gap without changing cleanup:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest scripts.test_verify_claude_stop_completion
```

The background session was `session-GTiqtn`, conversation
`0676488f-46a4-46c3-8866-efd87d9d63c6`. Its transcript reports `claude-opus-5-5`,
exactly two Bash calls with `run_in_background: true`, each sleeping 12 seconds
and printing its final probe token, and no other tools. It records both unmarked
interims, both marked finals and the intentional unmarked final.

| Request | Immutable bound official turn | Selected official continuation |
| --- | --- | --- |
| ask | `1c676f51-61dc-420d-bce7-ed4565cc953f` | `36bf6e7e-d4a3-4fdf-bef1-ff66ebabc1dc` |
| tell | `b15ab8ad-5cf0-4ab4-bbfd-5648981b21b7` | `5de8b7fd-c404-445f-8071-e5ccc1f48c2a` |

The ordinary self-test session `session-loqeaT`, denied-readiness session
`session-ewnqfJ`, configured-Stop sessions and background session are all closed.
Only task-owned sessions were closed. Verification did not install a binary or
plugin, change global settings, approve a provider trust prompt, or retry uncertain input.
Local raw logs and exact request records are under
`/tmp/tabcli-249-macos-20261023/` on the verifying Mac.

At `7c69580`, serial all-target/all-feature tests passed **1,483**, with **6
ignored**. Default/all-feature Clippy, Linux and MSVC cross-target Clippy,
formatting, diff and language checks passed. Cross-compilation and ignored live
tests are not authenticated Windows runtime evidence.

## Review corrections and final local gates

Independent source review found that the public minimum-version check and help
still admitted Claude versions which native completion refused. Commit
`5487a1d9447f51290b0d34adc8ea1820247f3d0b` aligns preflight, doctor and help with
stable 2.1.302 or newer. Claude owns the stable-release rule; other providers
retain their previous minimum-version behavior. Recorded launch eligibility and
actual module readiness remain separate gates.

Public admission, doctor and help regressions failed before that correction and
passed afterward. The final local Rust suite passed **1,488**, with **6 ignored**.
Both Clippy configurations, Linux and MSVC cross-target Clippy, formatting, diff
and language checks passed again. Python 3.12 discovery ran **40 tests**, with
**2 skipped**, including the five new probe tests. CI and independent shipping
verification belong to their exact PR head and are tracked on the PR; the
authenticated observations above remain explicitly attributed to `7c69580`.

## Protocol and evidence boundaries

New macOS sessions use the existing Claude native completion module and the
official cross-session input path, including the initial prompt. They require
the module's actual startup readiness before sending. Version eligibility alone
does not prove that Claude loaded the module.

This change preserves the marker protocol from
[the October 22 correction](2026-10-22-windows-260-reverification.md).
It does not prove general request finality or provider-owned continuation
parentage. A marked interim can still finish early, an unrelated turn in the
same conversation can repeat the exact marker, and an unmarked genuine final
can remain pending after caller timeout.

Claude 2.1.309 generated the inspected `claude-code/index.d.ts` with SHA-256
`8c37b2b5422de9af9e1a1e8689acc32f4d6e1e38c60e59db92ed0221fc79c770`.
The inspected `TurnStartInput`, `TurnCompleteFields`, `PromptOrigin`, and
`PromptSubmitInput` declarations supply no addressed request-final identity or
continuation-parent relationship that replaces that protocol. This inspection
does not establish the absence of every possible upstream facility.

An absent completion-mode record and explicit `legacy_stop` remain historical
formats. Already-pending legacy completion remains readable; new legacy tells
are refused. macOS recovery is inspection, appropriate close, and a fresh `ask`
with the needed context. macOS `reopen` is still unsupported.

This macOS round did not execute Claude 2.1.302, authenticated native Windows,
steering, combined queued input, `/clear`, worker respawn, interruption, API error,
refusal or oversized provider answers. Existing deterministic coverage and older
dated runtime records remain separate evidence. The minimum version is an
eligibility floor; the new macOS runtime observation here is specifically 2.1.309.
