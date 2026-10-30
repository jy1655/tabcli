# macOS verification of Claude caller identity removal (#335)

Verified on 2026-10-30 on macOS 26.5.2 (25F84), Apple silicon, iTerm2 3.7.6.
The owner selected the narrower caller-identity subset: keep execution settings in direct
non-Claude provider commands. No user settings or trust stores were edited, and the installed
`tabcli` binary was not replaced. All five sessions created by these checks were closed.

## Change and identities

The Claude adapter owns the ten identity names selected by Codex, Agy and Pi. The existing
Claude removal list and own-origin diagnostic are unchanged. The subset excludes `AI_AGENT`,
`CLAUDE_EFFORT`, `CLAUDE_CODE_ENTRYPOINT` and `CLAUDE_CODE_EXECPATH`. The helper union still
uses Claude's full list; this change does not restore hints removed by a terminal or helper.
The three adapters select both owned lists explicitly, and the Codex queue uses its
adapter's launch/probe selection. No provider transport or completion protocol changed.

- Base: main `ba73a2a43704a7b6fbe77a79f3c686fb2319c4e8`.
- Installed published `tabcli 0.4.32`, SHA-256
  `e4a15b86dc8897653d95d738effaa49281ae59fa4d2307f219a114ad465bb234`.
- Candidate: debug `tabcli 0.4.32` built from `fix/335-claude-caller-markers`, copied before
  the checks, SHA-256 `766bad0b71c6974e7b503555ba51a9e4251759c1bc063c990545fbe3af028729`.
  This is a candidate of the fix, not another published release.

## Deterministic regression

Before changing production code, four updated tests failed against the base: the Codex,
Agy and Pi probe tests and the Codex queue command test. Their fixed expectations require
all five Codex caller names plus the ten Claude identity names. All four passed after the
fix. The four complete launch-plan tests also passed, including both platform branches and
the applicable consent/model/effort/permission combinations. The tests pin the exact names
independently of the production lists and retain configuration/credential sentinels.

## Real child processes with synthetic provider executables

The public `doctor --provider PROVIDER --probe --json` command ran with small executable
fixtures first on PATH. They reported a version, recorded only known synthetic names and
match booleans, and exited for unsupported capability queries. This observes the real Bridge
command/environment path and OS process inheritance; it is not an authenticated provider test
or evidence that the fake capability queries passed.

The parent supplied synthetic values for all ten Claude identity names, the four retained
execution hints and an unrelated positive inheritance control. Each observation matched the
expected synthetic values; no real environment values were logged.

| Binary | Target | Child observations | Identity names removed | Execution hints kept | Control kept |
| --- | --- | ---: | ---: | ---: | --- |
| Published 0.4.32 | Codex | 3 | 0/10 | 4/4 | yes |
| Published 0.4.32 | Agy | 1 | 0/10 | 4/4 | yes |
| Published 0.4.32 | Pi | 1 | 0/10 | 4/4 | yes |
| Candidate | Codex | 3 | 10/10 | 4/4 | yes |
| Candidate | Agy | 1 | 10/10 | 4/4 | yes |
| Candidate | Pi | 1 | 10/10 | 4/4 | yes |

## Authenticated provider runtime

Ordinary state root and the existing repository workspace/consent, `--terminal iterm2
--yolo`, no model or effort override. The self-tests verify initial input, an exact result,
follow-up input, a second exact result and close. They do not independently test direct caller
inheritance because their helper commands already remove the union.

| Binary | Provider version | Session | Result |
| --- | --- | --- | --- |
| Published 0.4.32 | Agy 1.4.8 | `session-mLcgSE` | all five self-test steps passed; closed |
| Published 0.4.32 | Pi 1.4.0 | `session-w2WB7v` | all five self-test steps passed; closed |
| Candidate | Codex 0.166.0 | `session-lyNrC8` | direct ask/tell returned distinct exact answers; closed |
| Candidate | Agy 1.4.11 | `session-VvLgVs` | all five self-test steps passed; closed |
| Candidate | Pi 1.4.0 | `session-tQMHoI` | all five self-test steps passed; closed |

The Agy version changed between the two runs; this is not a same-version performance or
causal comparison. The full serial fixture suite ran concurrently with some of these checks;
no latency claim is made.

The direct Codex caller supplied all ten synthetic Claude identity values, a synthetic
`AI_AGENT` and an unrelated positive control. Initial and follow-up results were respectively
`AB335_INITIAL_OK` and `AB335_FOLLOWUP_OK`, with different request ids. During `tell`, a sampler
read only descendants of that sender and selected the real `codex queue` process. Five
readable samples contained the positive control and retained `AI_AGENT`, and none contained
any of the ten identity sentinels. Raw process environments were never saved. This is bounded
sampling of the queue process, not a claim about every descendant, hook or daemon.

## Not verified

- Native Windows runtime of this fix, including #111's cold start and later user-opened tab.
- Every terminal, permission mode, failure/interruption path, hook or provider descendant.
- Whether inherited Claude markers previously changed daemon startup, job attribution or
  another provider's behavior. The observed defect is their unintended inheritance.
- A fresh Claude provider session for this candidate; its removal list, messenger and
  diagnostic are unchanged and covered by the existing deterministic suite.

The published baseline remains installed. This record does not establish merge, publication
or installation of the fix.
