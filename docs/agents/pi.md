# Current Pi Boundary

- The Pi-installed extension reads core `turn.claim` and `cancel.json` read-only as a
  declared exception to the adapter-private record boundary. The whole UTF-8 claim,
  trimmed, is the current Claim token and currency guard; a changed or absent token
  ends correlation and cancel authority.
  Cancel is a JSON object with schema 1 and a string `claim_token`; other fields are
  ignored and parse failure supplies no intent. Claims can end through paths the adapter
  does not observe while Pi may still run (2026-10-15 critique). A Pi-owned copy would
  require retirement notification on every release path and a two-record publication
  protocol to avoid retaining abort authority. Tests execute the extension with real core
  writer output and round-trip its output through Rust readers. Replace these core reads
  when Pi provides an addressed interruption endpoint and TAB has an adapter claim
  retirement boundary.
  Both reads take TAB's core read policy (owner's decision, 2026-11-16): a link at the
  record's path, dangling or not, or any entry that is not a regular file is refused, and
  the bytes are decoded as fatal UTF-8; a refusal reads as no Claim or no cancel intent.
  The extension is copied into the session directory at launch, so a Pi session already
  running keeps the extension it was launched with, which follows a link and decodes
  invalid UTF-8 lossily, until it is relaunched.

- Native Windows Pi input has no verified addressed first-party endpoint with per-turn
  acceptance. TAB requires the installed extension's actual schema 2 startup record
  with `input_ack_schema: 1` before one console attempt, never a version inference.
  The extension matches the complete framed prompt's SHA-256 and UTF-8 byte length to
  Pi pending state at `before_agent_start`. Reading the whole trimmed core `turn.claim`
  at the correlated `agent_start` is also a declared read-only exception: it witnesses
  historical submission, without acquiring result or cancel authority. Latch one receipt
  publication attempt per claim before writing `pi-input-accepted-<claim>.json` through
  exclusive temporary creation and rename. Retirement after the witness read or a Pi
  conversation change does not invalidate that history. Retain receipts through later
  installs, rollback and close; they grant no input, result or abort authority. Sent
  requires a matching receipt within the original deadline. Read it before interpreting
  retirement or close, then re-read once; later publication can still leave Uncertain.
  Never resend text or Return. This fallback assumes no concurrent input and cannot
  distinguish a manual exact copy of the framed prompt. Keep open sessions on the same
  or a newer TAB binary, including `AGENT_BRIDGE_EXECUTABLE`: older follow-ups can
  strip the startup input capability. Replace this fallback when Pi exposes addressed
  interactive input with per-turn acceptance identity.
  A Windows Herdr terminal keeps this contract unchanged (the same framing, startup wait and
  receipt); only the attempt is Herdr's `terminal.send`, whose refusal before typing is not
  sent and whose uncertain outcome is never resent. Live evidence is in the Herdr boundary.

- A Pi pending turn is installed (the launch plan while the session is Launching, and a
  follow-up before delivery) only for the current Claim of a session that has no close
  tombstone and whose stored state is not ended; the rollback cancel removes the exact
  Claim's pending record in any state. Decided 2026-10-30 after the Claim-currency guard
  (#324) made the token-only installation visible.
- Pi v1.0.4 exposes `ctx.abort()` to an interactive extension, but no verified official
  endpoint addresses that running interactive session from outside. Cancel therefore
  reaches the official API through a Pi-owned record and polling fallback. Replace the
  polling and acceptance record when Pi provides that addressed interruption endpoint.
- A captured context resolves the runtime at call time; it does not identify a turn.
  Start the 200 ms timer at `agent_start`, invalidate its run before clearing it at
  `agent_end`, `agent_settled`, shutdown or a new run, and require both the current Claim
  and its correlated prompt before aborting. A callback of an old run stops nothing.
  The interval is not a response-time guarantee.
- Call abort at most once for a Claim. The call and the atomic publication of
  `pi-cancel-accepted.json` cannot be one operation; a failed publication never permits
  another call. Acceptance is not completion. Only the same Claim's accepted call,
  the last assistant's abort outcome at `agent_end`, and matching cancel intent permit
  a cancelled Result at settlement. Recognize `stopReason: "aborted"`, or `"error"`
  with exactly `errorMessage: "This operation was aborted"` (tool abort observed on
  Pi 1.0.4, 2026-10-11). Do not classify other errors by an abort substring. Direct Esc
  stays an ordinary failure and identifies no actor. Exit before reporting keeps the
  ordinary recovery semantics.
- The installed extension proves cancel capability with schema 2 startup readiness.
  The adapter carries that evidence to the next Claim and clears its acceptance record
  at pending-turn installation or rollback. Never upgrade a schema 1 session by version
  inference. Other modules do not read Pi's acceptance record.
- Retry and compaction can settle without a new aborted `agent_end`; record what Pi
  reports, not what the cancel intended. Pi returns queued messages to the editor on
  abort. TAB uses no Pi queue and deletes no editor input. Tell the user to check
  the surface before another `tell`, whose paste can combine with restored input.
- Usage comes only from the signal Pi documents as usage: the `usage` of every assistant
  message, which the extension sums over every `agent_end` of the correlated claim until
  settlement (`source: pi_extension_agent_end`, `scope: request`). Tool-result usage,
  compaction summaries and Pi's computed `cost` are excluded; `reasoning_tokens` only when
  every message reports it; `coverage: complete` only when every run's final assistant
  message stopped ordinarily. A malformed operand or sum becomes a bounded `usage_error`
  and no partial sum. Usage never changes the outcome, and the size policy drops usage
  (`usage_error: "omitted: event size"`), then the usage error itself, before it fails a
  message that fits on its own.
  The event fields are optional and omitted when absent, so an older binary reads a new
  event but drops the fields and sees a journaled completion as `Mismatched`: do not
  downgrade the installed binary or `AGENT_BRIDGE_EXECUTABLE` while a session whose turn
  this binary journaled is open; upgrading is supported. Replace the accumulation when Pi
  reports a turn's usage through an addressed first-party signal.

