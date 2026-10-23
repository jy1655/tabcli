# macOS follow-up verification, 2026-10-23

These observations close specific live-verification gaps listed under "Not verified" in [the 0.4.24 report](../releases/0.4.24.md). They do
not replace deterministic tests or establish behavior on native Windows.

## Installed 0.4.24

Operator observations from this task's version, SHA-256, consent-record and console-state commands follow.
Executable: `/Users/jy/.cargo/bin/tabcli`, reporting 0.4.24. SHA-256:
`52219c3635d11f96ddb0a4de413dacd41110c7050891011a175de2394676e527`.
Workspace: `/Users/jy/Developments/ForTest/agent-bridge`; ordinary state root, existing
verified workspace consent, explicit provider bypass (`--yolo`). The operator did not answer a provider approval
or edit a trust store, installed binary or plugin.

Before Ghostty, the console session was logged in and `CGSSessionScreenIsLocked` was
absent in `ioreg -n Root -d1 -a` (the locked state sets it to true). Ghostty 1.3.1,
iTerm2 3.7.6, Codex 0.163.0, Agy 1.4.5.

| Actual provider/surface | Result | Owned session |
| --- | --- | --- |
| Codex / Ghostty | All five steps passed: ask, initial result, tell, follow-up result, cleanup; 22.272 s | `session-N8NXZf` |
| Agy / iTerm2 | All five steps passed; 32.080 s | `session-7bINrK` |

Both sessions report `closed` and successful cleanup. Codex's requests were
`request-1792733548567899000-18876-0` and
`request-1792733561903427000-20339-0`; Agy's were
`request-1792733617401954000-21792-0` and
`request-1792733636430798000-23519-0`. Each exact addressed request has its own
published Result. Private self-test JSON and session records retain the evidence.

Both changes are in v0.4.24. From the production code paths, Ghostty startup uses the
moved frame host (#287), and a successful Agy macOS `tell` uses the moved log lexer
and requires its input receipt (#293). This is a code-path inference from the completed
round trips; it does not exercise the
Windows console client (#278) or default-mode RunCommand approvals.

Not verified here: guarded dialog precheck (#279) against an actual provider trust
dialog; locked-screen behavior or the cause of the earlier Ghostty timeout; Agy's
native Windows follow-up/default approval mode; unrelated host/provider combinations.

## Terminal.app incarnation candidate

The refactored candidate was built with `cargo build --locked` from source `5950c029dd62928551c5a3a4b7c5e2aea158c22e`
(the implementation plus main's #282), without replacing the installed executable.
Candidate SHA-256:
`fe6326fd7a144a3697ddffd9dff417ae7234d26de231709136f1bf05d47ad429`.

After confirming the console was logged in and unlocked again, Codex 0.163.0 on
Terminal.app passed all five self-test steps in **23.709 s**, using the same ordinary
state root, workspace and `--yolo` mode. Owned session `session-JrWXp1` reports
`closed` and successful cleanup. Exact requests:
`request-1792734887050993000-58581-0` and
`request-1792734900957847000-61051-0`.

This checks an ordinary launch/follow-up/close of the changed binary. App restart,
PID reuse, ambiguous instances and every deadline transition remain deterministic-test
evidence; this run did not manufacture those runtime races or a fresh consent dialog.

## Installed host integration

This Codex host used the installed Agent Bridge 0.4.24 skill, read from its plugin
cache, for actual Claude 2.1.309 work on iTerm2. In owned `session-8zDz6z`, the
implementation request `request-1792733516787365000-16564-0` completed, followed by
the separate evidence-review request `request-1792734299637089000-45475-0`. Both
addressed results were read before `close-session --explicit` confirmed `closed: true`
and `ok: true`. This is an authenticated installed-skill ask/tell/result/close path,
not a claim about other hosts or automatic skill selection. No integration was installed
or updated in this task.

Bridge's close report exhausted its bounded scan of the historical session registry
(`time_budget_exhausted`, no open descendants reported). The delegated prompts forbade
further delegation and the returned work was inspected; the named managed session's
own close was confirmed. This does not establish a complete registry-wide descendant scan.
