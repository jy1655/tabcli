# Pi Windows input acknowledgement, issue #254

This records the implementation owner's native Windows evidence from 2026-10-19 at
`274244ee6e5da145945f3759eb2779ce34d2564a`. It is not independent final validation.
The subsequent review corrections change documentation, one comment and reset ordering;
no protocol, transport or state behavior change is intended.

The owner reproduced the parent's false Sent behavior on both initial and follow-up input
with pre-fix tabcli 0.4.18 and Pi 1.3.0. A diagnostic extension used Pi's official terminal
input callback to remove exactly one Return. Bridge reported successful delivery while
the complete framed prompt remained in the editor and no corresponding `agent_start`
occurred. The control removed no Return and completed both requests. This controlled
trigger establishes the reporting defect, not a spontaneous loss rate.
It used the installed pre-fix binary; an exact-parent rebuild remains part of independent
final validation.

The implementation owner then ran seven passing cases with Pi 1.3.0, Node 22.23.3,
`--terminal windows-console` and `--yolo`. All eight owned fixtures were closed, including
one failed close-wait evidence collection attempt followed by a fresh passing fixture.
Post-close inspection reported every owner absent and no open or incomplete descendants.
No input was retried.

| Owner case | Observed outcome |
| --- | --- |
| Initial Return removed | Full draft retained, no start or receipt, Uncertain at the original deadline. |
| Follow-up Return removed | Initial request completed; follow-up draft retained with no new start or receipt, Uncertain at the deadline. |
| Control | Two exact results and matching receipts, no removed Return. |
| Large Unicode initial and follow-up | Both completed; actual Rust pending fingerprint, Pi-observed prompt and receipt agreed at 41,079 and 41,080 UTF-8 bytes. |
| Immediate next request | Two exact results and separate retained receipts. |
| Close while waiting | Full draft and no receipt; closing ended the wait as Uncertain before the deadline. |
| Old extension | New-head follow-up to a session started by 0.4.18 refused before console input and requested a new session. |

The large prompts included Korean, emoji, actual tabs and newlines, literal escapes and
quotes. This comparison used the actual Rust-written fingerprint, not reconstructed framing.
An earlier owner's prerequisite control used different prompts of 34,545 and 34,546 UTF-8
bytes and reconstructed framing; it is separate evidence.

The owner recorded 44 passing Pi-focused Rust tests on Windows, including the Node tests
that run the shipping extension with Rust-written core claim and Pi pending records and
read its receipt through the production Rust reader. Existing cancel and usage cases
passed. Host and Windows-target clippy passed; Linux and macOS clippy were cross-compilation
only, with no provider runtime evidence on those platforms. Older Pi event-shape compatibility
rests on primary source and legacy-shape fixtures; only Pi 1.3.0 ran live for this change.

Receipts deliberately remain historical records through close, unlike cancel authority.
They grant no result, input or abort authority. Exact manual copies are indistinguishable,
and the console fallback assumes no concurrent input. Receipt publication after the one
retirement/close re-read, or evidence first observed after the deadline, can leave delivery
uncertain even when Pi started the prompt. Detached Windows commands can wait the full
original deadline. The compiling red commit also includes the equivalent initial-directory
lookup refactor to `TurnContext` needed for the transport test seam.

Independent final parent/head live validation, the full serial suite and lint checks at the
final head, and Windows, Linux and macOS CI remain pending. Root will add their results
after that evidence exists. Neither source review nor cross-compilation discharges those
runtime checks.
