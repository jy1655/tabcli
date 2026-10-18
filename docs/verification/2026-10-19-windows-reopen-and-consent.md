# Windows reopen recovery and workspace consent, 2026-10-19

Native Windows 11, PowerShell 7.5.4, Windows Terminal 1.24.12741.0 and Claude Code 2.1.302.
All managed launches used `--terminal windows-console --yolo --prompt-file` and authenticated
Claude Opus at high effort. Tests used task-owned sessions and preserved the user's terminal.
Raw transcripts, prompts, local paths and provider identifiers remain private.

## Reopen launch failure (#248, #257)

An independent Opus verifier built both the parent `97c2042c13df27bb2f7231cd2e03d7ea7a617cfb`
and final head `60f778888fa4b83f97b4f24d51bd845ab58f2f7e` with `cargo build --release --locked`.
The binaries still reported 0.4.18; these are the source changes included in 0.4.19.

The verifier created and closed a real Claude conversation, then removed PowerShell discovery
from the PATH of one reopen process. This produced a genuine terminal-launch failure after
source reservation and before any provider process or terminal was recorded.

| Observation | Parent | Final head |
|---|---|---|
| Launch failure | Untyped error; source stays reserved | `launch-failed`, schema-3 refusal bound to the launch receipt |
| Retry | Refused as `already-reopened` | Successful initial response in the same Claude conversation |
| Closed source history | Unchanged | Unchanged |
| Owned sessions | Closed | Closed |

The final head's native serial suite passed 1,101 tests with 5 ignored. Clippy, fmt, diff check,
Linux/Windows target checks and all six jobs in CI run 39146331138 passed. Independent Opus
correctness review and [runtime verification](https://github.com/jy1655/tabcli/pull/257#issuecomment-6252584562)
passed. The Windows Unknown-process fixture disables debug privilege only in its isolated test
process, so CI tests an actual access-denied observation.

The live case covers the Pending receipt with no process. The spawn overlap, Gone/Live/Unknown
identity cases, malformed records and typed-gate precedence have component coverage. A wrapper
exit or closed surface alone is never proof that a provider process is gone. No general
cross-platform live claim follows from this test.

## Claude workspace consent (#28, #258)

The user first approved two new exact canonical workspaces: one through Codex outside Bridge,
one through a managed Agy session. Subsequent tests reused those genuine approvals; they did not
create approval evidence. Each Claude run received a protected private configuration from a
hash-verified copy of an unmodified provider backup that did not trust the target. The original
backup had rotated away; the copy matched its earlier recorded hash. Provider settings were
copied unchanged. No global trust or permission setting was changed by these runs.

An independent Opus verifier built actual parent `53962ad7d4d83c12b0ba7b35187176be65fa11e4`
and final head `66f4e442d76ecfc1103007adfa327364f26e2d55`. Both binaries reported 0.4.18.

| Case | Observed result |
|---|---|
| Parent, Codex-origin approval, Terminal tab | Initial delivery failed; Claude workspace trust remained false; no applied marker |
| Head, same origin, fresh Terminal tab | Exact requested response; Claude trust confirmed; `provider-workspace-dialog` applied |
| Head, same origin, native console fallback | Same success; no tab-host record; `wt.exe` excluded only from this process's PATH |
| Head, managed Agy-origin approval, fresh tab | Exact requested response; recorded source is Agy; Claude trust confirmed and applied |

All four fixtures closed and their copied credential files were removed. The verifier checked
global trust facts and provider-store hashes before and after each case. No uncertain input was
resent. The final rebased head passed 1,117 native serial tests with 5 ignored, Clippy, fmt, diff
check and both target checks. Independent Opus code and comment reviews passed; the final
[runtime verdict](https://github.com/jy1655/tabcli/pull/258#issuecomment-6252825177) is PASS+NOTES.

Earlier owner runs on the same production behavior also passed two fresh tabs, a native console
and one tab while two owned CPU workers were active. Closing a fixture or revoking its isolated
Bridge consent before dispatch withheld the response. These runs predate the final diagnostic
wording corrections; they are separate from the final-head independent verification above.

One second of unchanged recognized dialog pixels is a bounded workaround for the observed Claude
input guard and selector remount. It does not prove UI-generation identity or atomic readiness;
concurrent typing is outside its assumptions. The response remains one attempt under the
original claim and deadline. Only Claude's trust store confirms success. Arbitrary dialogs and
permission prompts receive no approval. macOS live behavior is not verified here, so #28's
cross-platform completion remains open.
