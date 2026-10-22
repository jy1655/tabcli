# Architecture

Follow an `ask` from launch preparation to a retained result to find the module you need to change.
The Rust CLI manages one provider conversation per session in a visible surface. A request crosses
these boundaries:

```text
native.rs: dispatch into session_start, which parses ask, checks inputs and prepares the
  session directory
  -> session::turn + session::requests: acquire the initial claim and write its receipt
  -> provider + terminal: prepare the CLI and bind a newly created surface
  -> session::launch::launcher: write the launch receipt, run the bootstrap in the bound surface,
     wait for the confirmed startup
  -> session::launch::wrapper (inside the surface): admit the wrapper, record the owner, take the
     adapter's complete provider command, spawn it, finalize the launch record
  -> turn_run -> delivery: the Turn run uses the adapter-declared Transport and settles delivery
     of the claimed turn
  -> provider: recognize the matching completion
  -> session::turn: write the completion journal
  -> session::turn: publish the result, update status, release the claim, remove the journal

result / inspect
  -> query: read the published result without changing records

close-session
  -> session::close: claim the surface handle for close
  -> terminal::close_authority: decide close authority over the observation seam
  -> terminal::ownership and the platform adapters: act on the owned surface
  -> session::close: settle the close records and retain the tombstones
```

A follow-up enters through `native.rs`, which dispatches into the `tell` module; `tell` parses
the command and claims the existing session before delivery.
The admission checks read Hold without taking another lock; `claim_ready` identifies follow-up
claims so the final `begin_delivery` hold gate excludes initial delivery.
After its admission checks and the provider-specific reopened-conversation refusal, `tell` hands
the claimed follow-up to `turn_run`; session-start hands it a launched, consent-completed initial
turn. Both callers keep their early report addressing. `turn_run` composes delivery settlement,
the optional Claim-addressed wait and success rendering. Follow-ups skip initial readiness and
launch gates. Preparation, optional surface checks, remaining timeout and budget precede
`begin_delivery`; no lifecycle lock is held across the transport operation. Detached commands
still await delivery settlement and skip only result waiting. Both paths use the same completion
journal and publication path. If delivery is uncertain, the claim stays held; neither a query nor
repair resends the prompt.

A record is durable evidence of an operation, not a disposable cache. Use the terms in
[CONTEXT.md](../CONTEXT.md) when reading the lifecycle code. The rules for changes live in
[AGENTS.md](../AGENTS.md).

## Module map and dependencies

- `src/main.rs` contains top-level dispatch and help; `src/native.rs` holds the native command
  table, the dispatch, session creation and the delegation prompt, and declares the native
  modules. Each command parses and runs in its own module: `ask` in `session_start`, `reopen`
  in `reopen`, and `tell`, `sessions`, `close`, `prune`, `hook`, `console_control`, `hold` and
  `cancel` in the modules of those names; `args.rs` holds the shared argument helpers and the
  explicit-workspace resolution. No test reaches another module's internals through
  `native.rs`. After their pre-checks, `tell` and session-start hand the claimed turn to
  `turn_run`. The launch protocol is two operations in `session::launch`: `launcher.rs`
  holds the start-in-surface operation and both bootstrap builders; `wrapper.rs` holds the
  native-session wrapper with its admission and provision steps, the fd handoff consumer,
  provider resolution and the version gate; `mod.rs` keeps the launch records and their rules
  (receipt, phases, failure handoff, repair, wait, spawn, finalize).
- `src/native/process_identity.rs` classifies process evidence as Live (Incarnation or PID-only
  compatibility), Gone (Exited or Reused), or Unknown. It performs no OS or record reads;
  platform adapters own absence and identity observations. A reused incarnation grants no
  Surface authority.
  Terminal adapters, Reopen provider observations, and the Claude registry use
  `process_identity::classify` for incarnation comparison; messages and authority remain
  adapter-owned.
- `src/native/process_env.rs` owns child environment policy and deadline-bounded command execution;
  provider-specific execution remains in its adapter. Non-provider children on macOS and
  Windows remove the union of adapter caller-session markers and Bridge's four session
  variables; helpers and the bootstrap explicitly supply the context each child needs.
  Windows Terminal and both console roots use the same filtered environment block.
- `src/native/delivery.rs` consumes one Claim and owns readiness, injected reopen gates,
  prompt-file framing, surface ownership verification, remaining timeout and send-budget
  checks, begin/send/settlement, and initial-prompt cleanup. Provider adapters explicitly
  declare initial and follow-up Transport values with `facts`, `prepare`, `send`, and
  `discard_prepared`. Only known NotSent discards preparation; Uncertain retains the Claim.
  `LaunchArgument` settles Sent without reading the prompt or repeating wrapper work.
  The terminal module owns the Windows submit-delay budget; Transport facts supply the count
  and provider-selected console newline encoding. The console helper resolves both from
  the verified session's provider; its CLI carries the session identity and prompt file name.
  The surface-ownership timeout, remaining-timeout and paste-framing helpers
  (`verify_terminal_surface_ownership_until`, `remaining_turn_timeout`, `terminal_input_bytes`)
  live here now; `tell`'s pre-check imports the first two.
- `src/native/turn_run.rs` runs one Turn from a held Claim to its Result for `ask`, `tell` and
  `reopen`: it repeats the report address idempotently, composes delivery settlement through
  `delivery::deliver_with`, the optional Claim-addressed `turn::wait`, and success rendering.
  `emit_session_result_with` lives there. The caller's delivery-error hook is called exactly once for
  a Delivery failure; wait and emission errors never reach it. `run_with` takes the Transport and
  the `Checks` so the tail is tested at its own interface with the shared fake Transport.
- `src/native/turn_report.rs` keeps `TurnReport`: address-aware failure finishing. Callers call
  `TurnReport::finish` outside the Turn run, with their own extra fields.
- `src/native/session_start.rs` owns the original start deadline and the Initial prompt
  operation for an already prepared Session. Fresh and Reopen resolution and creation finish
  before that operation; their explicit `InitialSource` implementations supply result fields,
  holder gates, and phase-specific failure handling. Consent failure remains separate from
  launcher and Delivery failures.
- `src/native/reopen.rs` owns Reopen resolution, provenance, and the initial operation's
  integration with `reopen/reservation.rs`, which owns source reservations, launch refusals,
  and their settlement, including read-only marker diagnostics. Session owns the refusal record
  shape. Reopen creates the new Session and records its provenance before finalizing the source
  marker. The resulting `ReopenAttempt` owns settlement after the Initial prompt operation,
  including failures before a Request address exists. Launch and delivery keep their existing
  holder-check timing; provider adapters supply the holder evidence.
  `Reservation::claim` rechecks the closed source under its lock, and consuming `finalize`
  leaves its marker in place; an unfinalized Drop releases only its own marker. Settlement
  preserves the durable refusal re-read and checks provider-process evidence before release.
  `reopen/gate.rs` owns typed `ReopenGate` reasons, their unchanged JSON spellings and
  recognition through error context. `reopen/reservation/marker.rs` observes the marker,
  its named Session and refused-launch cleanup once,
  then maps that Judgment to command eligibility or doctor diagnostics. Only these callers
  request the cross-session observation. The command repeats it under the source lock before
  replacing a stale marker; doctor never acquires mutation authority.
- `src/providers/` defines the CLI identities, minimum versions, and model, effort, and bypass
  argument mappings. The native adapters call these mappings when they build their complete
  command; shared launch code no longer does.
- `src/native/provider/` owns each provider's launch and resume plans, trust interpretation,
  transports, completion correlation, hooks, diagnostics, and adapter-private records. Each
  adapter owns its provider home, resolved from the environment snapshot: the trust store, and
  for Claude the session registry, for Agy the brain root, for Codex the memories directory. Each
  adapter's launch and resume plan is the complete provider command (the policy flags in the fixed
  order, the adapter's own arguments, the Initial prompt's place), the environment removals, the
  initial-prompt disposition the wrapper applies after the spawn, and an unstarted completion
  watcher the wrapper starts and stops; `provider/mod.rs` holds only provider-neutral contracts.
  Claude's cross-session messenger (the follow-up Transport, the Windows initial transport, and
  the `message-guard` and `message-receipt` controls) is its own module under `claude/`, built
  per send from a clock and a retry policy so that its timeout and retry tests inject both; the
  adapter keeps launch, resume, trust, hook completion, Reopen and doctor.
  Pi's Windows input operation owns its receipt-first polling and final retirement recheck.
  Its internal runtime supplies console dispatch, Record observations and the existing Clock;
  tests substitute that runtime while retaining the real Record readers and delivery settlement.
  It uses the session seam and terminal operations. Keep provider schemas and transport selection
  in `src/native/provider/<provider>/` (or a single `<provider>.rs` file), not in shared
  orchestration.
- `src/native/console_control.rs` owns the Windows helper's argv, payload, reply, and failure
  protocol; its private client owns process invocation, private payload lifetime, and asynchronous
  window lookup. `Client<H: HelperProcess>` owns the dependency for send, text-only send, screen,
  guarded dialog/submit, and close. Production constructs it with `installed()`; each test gives
  its client its own helper, with no thread-local selection inside the process adapter.
  Codex's Windows paste holds one client across its text, screen and submit operations; its
  portable recognizer/protocol tests still run on every platform. `WindowLookup` retains its
  separate asynchronous child, polling and Drop cleanup.
  Command parsing builds an opaque `ConsoleControlRequest`; after session and
  surface verification, provider facts resolve it to an action for the Windows executor. The
  in-process test adapter uses the same decoder, provider resolution, and reply interpretation.
- `src/native/terminal/` selects and implements platform-specific surfaces, input, observation,
  and close. Windows retains console attachment, Win32 effects, and lifecycle-locked input dispatch.
  Its `close_authority` module owns the observation seam (the Records read when the decision
  asks, and the OS and surface observations), the decision and the dispatch, with a production
  adapter and an in-memory test adapter; `ownership` keeps the owner record types, control
  proof and the owner-teardown action, and the platform adapters keep the surface close.
  The macOS owner wrapper reads the record and calls the same ordered verifier as portable tests;
  live-process and TTY observations are lazy so that an earlier refusal prevents later OS reads.
  `session::owner` owns process observation and
  attestation. The Terminal.app adapter owns app incarnation. Process reuse alone never
  supplies control proof or ordinary dead-owner Surface authority.
  Each terminal kind is registered once: its portable `facts.rs` declares its surface facts,
  host command, close-evidence choice, refusal messages for screen reads and guarded dialog
  input and, on macOS, its binding facts, and the shared lookups (`facts`, `close_evidence`,
  `host`, the command table) read that registration. The installed macOS dispatch is a value
  built from each adapter's injectable dependency (the AppleScript automation, the Ghostty and
  Warp runners, the WezTerm host); a test builds one with a recording automation or a fake
  runner. The shared macOS operations refuse an operation the kind declares unsupported from
  its facts before any adapter is reached, so an adapter implements only what its kind
  supports. Shared `open_with` preserves binding and cleanup order; Warp owns its open flow.
  `applescript` owns execution and common response vocabulary, while each adapter owns its
  screen and presence scripts.
  iTerm2 and Terminal.app carry an internal Automation dependency. Osascript owns execution,
  deadlines and delivery-failure classification; scripts, arguments, ordering and close
  authority remain adapter-owned. Tests inject recording or script-executing Automation
  implementations through the dispatch they build. The Windows submit budget and the macOS
  terminal selection live in portable children of their platform modules (`windows/budget.rs`,
  `macos/selection.rs`), compiled and tested on every target.
- `src/native/session/` owns durable record schemas, record access, and durability.
  `schema` defines the shared manifest, event, provider-process, reopen-marker and reopen-refusal records and
  their constants; `primitives` owns session environment names, identity validation, and time
  helpers. `close::budget` owns the close deadline scope and its optional test clock. `requests`
  binds public requests to claims and events; `turn` owns delivery settlement, completion and
  recovery; `close` owns
  close-record transitions and dead-owner repair; `launch` coordinates spawn, cancellation,
  and the full macOS binding wait, and holds the two launch operations: `launcher` starts a
  created session in a surface and `wrapper` runs the native-session command inside it.
  Terminal adapters supply surface identity checks and declare binding diagnostics and wait
  budgets.
  `enumerate` owns lazy state-root entry traversal and exact workspace/provider scope matching.
  It returns root-open errors separately from typed entry and file-type faults; callers retain
  their error policies and validation order. `sessions` repairs and `prune-sessions` deletes in
  encounter order; `status` and `search` collect ids and sort before observation.
  `state` owns the serialized status and its writes: `Store::update_status` applies
  permitted transitions and generations, while `Store::record_residual_surface` amends
  diagnostics without changing lifecycle fields. Both preserve closed tombstones.
- Within `src/native/session/`, start with `record.rs`: one decoder per JSON core Record
  (manifest, status and the closed tombstone, completion journal, launch record, Receipt), the
  byte-source seam and the Snapshot assembly, with three adapters: the live source (the shared
  lifecycle lock and the before/after check of the four state files), the timeline's cached
  source (read once, strict, bounded, whole-map recheck) and Origin's budgeted source
  (per-record cap, byte and time allowance, selective reads, no Snapshot). Every other reader
  of those Records (the lifecycle paths, reopen provenance, the Claude projection) calls the
  same decoders while keeping its own missing-record policy and error context. The auxiliary
  reopen-marker, reopen-refusal and provider-process shape decoders also live here, preserving
  their historical serde policy; the Snapshot timestamp rule is not added to those records.
  Reopen still judges refusal validity and release authority after decoding. Then `reads.rs`
  for event reads and read budgets, `primitives.rs` for filesystem durability and
  private-record permissions, and `owner.rs` for read-only attestation of the recorded owner.
  Owner attestation separates incarnation birth from recorded TTY/process-group control,
  preserves inspection errors, and projects the
  public OwnerObservation. It takes only an owner value and OS observations, never a Store
  or session lock. Caller policy for prune, repair, wait and exact close-intent resumption
  remains in session; control proof and the close authority table remain in terminal.
  R1 removes duplicated execution lookups, not the type dependency on terminal::ownership.
- `src/native/session/origin.rs` owns Origin observation. It selects a session's Origin only
  from the receipt of the claim its launch record names (from a read Snapshot or by a bounded
  direct read), and judges whether that one Origin, in the same state root, ended: a valid close
  tombstone first, then a recorded `closed`, `failed` or `exited` status, then
  `attest_owner(owner).process` for a bound owner, never the projected liveness bools. A missing
  Origin directory is undecided, not ended. It reads through `RecordReader`'s bounded strict
  regular-file reads within a `ReadBudget`, rechecks the tombstone and status after the verdict,
  and opens no Store, takes no lock, repairs, recovers or writes nothing. It renders the public
  `origin` and `origin_observation` fields once for `status`, `sessions`, `inspect` and `doctor`,
  and owns the bounded started-from scan that `close-session` reports after a successful close,
  with the re-verification of each listed session that `--with-started-from` runs before its
  close.
  The scan enumerates lazily and reads per session only the manifest, tombstone, status, launch
  record and one receipt; it never closes or repairs another session and never fails the close.
- `src/native/query.rs` and `query/timeline.rs` own command-specific observations and fields for
  read-only inspection, result, search, and timeline output using the session readers and common
  publication rules. Queries get a `Reader`,
  not a `Store`. To expose an adapter-private fact, add an observation to that adapter's interface;
  do not read its private record from the query or call repair.
  `query/request.rs` owns the Request observation, which precedes JSON rendering.
  `with_owner` is the only adjustment of that observation, and only `wait` applies it.
  `query/search.rs` owns search with its budgets.
  Context attachment resolution uses the same
  observation and requires strict event decoding, an exact event name, and verifiable provenance;
  it does not reconstruct Request meaning from command JSON. Compact rendering in `context.rs`
  is deterministic over the resolved message and the retained question emissions, read under
  the same Snapshot, and its mode and counts are recorded on the receipt. Timeline retains its cached strict
  reads, and search retains its cumulative byte budget.
  Codex lifecycle diagnostics read the typed Request state and event identity, release the
  Snapshot's shared lock, then perform their existing strict event reread for provenance.
  `query::observation::PartialObservation` owns doctor's fallback when no Snapshot can be read:
  Cancel and Hold retain separate lock attempts and failure scopes, with the best-effort manifest
  read after both. Rendering partial evidence does not repair it or grant control authority.
  Inspect, doctor, and `status` render one Observation per session from these readers and publication rules;
  owner process observations and surface probes are added on request, while `sessions` keeps
  its own lock-free listing after its optional repair. The Observation's judgments live in
  `query/observation/judgment.rs` as pure functions over the Records, with the owner attestation
  and the surface probe made on request and memoized per Observation; `wait` makes a fresh owner
  judgment per poll, `inspect`'s renderer is `query/inspect.rs`, and no renderer (status, doctor,
  inspect, wait) derives a rule of its own. All four add the Origin fields through the
  session origin reader; that reading stays observation-only even in the repairing `sessions`. Its `results` counts event paths admitted
  by the publication predicate used by status and result. `Reader::published_result_count`
  reads receipts and claim/journal evidence with a before/after consistency check, without
  locking, retrying, or decoding ordinary event bodies. The journal event comparison uses the
  existing 64 MiB bound (plus one byte to detect growth). Receipt indexing is read twice for
  consistency; a count read failure keeps the row with `results: 0`.
- `src/native/envelope.rs` owns shared envelope defaults, conditional parse-error JSON emission,
  and pretty JSON output. Commands retain their fields, override order, plain-text rendering,
  and error propagation. The module performs no session reads or writes.
  The schema-less success output of `prune-sessions` and `close-session`, and the reports of
  `doctor`, `consent`, and `self_test`, retain their separate formats. The `sessions` array and
  `settings` output are also separate and are not wrapped in an envelope.
- `src/native/doctor.rs` combines read-only shared observations with provider diagnostics and
  optional bounded probes. The diagnostic vocabulary (`Check`, `Availability`, `Context`,
  `ResultTimeoutDiagnostic`) lives in `src/native/diagnostics.rs`; provider adapters and the
  Reopen marker observation depend on it rather than on the command. `probe` stays in
  `doctor` for now. Providers choose which observation can explain a result timeout;
  `diagnostics` assembles that read and `self_test` bounds its execution. A diagnostic report is not
  proof of successful delivery. The report states its scope using the check inputs and existing
  observations.
- `src/native/self_test.rs` runs the explicit public self-test through bounded commands, checks
  initial and follow-up results, and closes only its own sessions. It uses the ordinary state
  root unless isolation is requested; it is not called implicitly by another command.
- `src/native/consent.rs` preserves the public consent path and re-exports four concerns:
  `consent/store.rs` owns the exact-workspace trust-store reader used by provider adapters;
  `consent/record.rs` owns Bridge's consent Record and command; `consent/launch_dialog.rs`
  coordinates launch dialogs through its `DialogSurface` seam and the provider and terminal
  adapters, consuming their declared `LaunchTrust` facts; and `consent/wait.rs` owns the one
  trust wait. Consent snapshots the caller's environment once per boundary and asks each
  adapter for a workspace's trust against the adapter's own home, or the recorded source store
  for a recheck; it knows no provider path. Provider stores are still read through consent's
  store reader and never written by Bridge. The wait checks the ended-session rule first and
  serves the Codex paste gate and the Agy delivery gate with their own readiness and messages.
  Its `SessionConsentObservation` preserves the raw diagnostic JSON while exposing known state
  and applied-marker accessors. Unknown or damaged diagnostic records are not authorization;
  the strict consent record and current-source revalidation remain separate. Doctor retains its
  later normal-path consent reread.
- `src/native/settings.rs` owns user preferences for surface creation and one launch admission
  preference, `codex-memory-workspace`. A preference cannot grant or remove close authority over
  an existing session or surface.
- `src/native/session_start/admission.rs` applies that admission preference to a resolved
  workspace for `ask`, `reopen`, and `self-test`, before any session record, consent record,
  provider check, or surface exists. It reads the caller's environment through the same
  `Environment` snapshot as workspace trust; the protected `memories` location is declared by the
  Codex adapter, and no other adapter takes part. A refusal writes nothing.

## Session lifecycle

Codex's `launch/lexical.rs` holds its pinned screen labels, numbered-label decoding and
confirmation-hint lexemes. Initial readiness, legacy submission and addressed draft
acceptance retain separate predicates. In particular, a hook-review label anywhere vetoes
injected readiness, while the same unnumbered words inside an addressed draft are content.
The paste module owns the span, footer, overlay and rendered-cardinality policies.

A launch creates a private session directory, records its manifest and initial request, and binds
one newly created surface. The wrapper records its owner and provider spawn evidence. Depending
on the provider, initial delivery is a launch argument or occurs after readiness in the running
surface. A created surface does not prove spawn or delivery: the wrapper can fail before it starts
the provider.

The serialized state strings and permitted changes are:

| From | To (in addition to writing the same state again) |
| --- | --- |
| `launching` | `running`, `awaiting-initial-input`, `failed`, `closed` |
| `running` | `ready`, `exited`, `failed`, `closed` |
| `awaiting-initial-input` | `working`, `exited`, `failed`, `closed` |
| `ready` | `claimed`, `exited`, `failed`, `closed` |
| `claimed` | `working`, `ready`, `exited`, `failed`, `closed` |
| `working` | `ready`, `exited`, `failed`, `closed` |
| `resume-pending` | `working`, `ready`, `exited`, `failed`, `closed` |
| `exited`, `failed` | `closed` |
| `closed` | None |

Hold is orthogonal to this state table: it refuses follow-ups whose delivery Bridge has not yet
allowed to start, without changing the current turn.

`resume-pending` remains readable for compatibility; it is not the modern `reopen` mechanism.
Unknown state strings remain readable, but cannot transition to a known state. Status writes
advance a generation; a closed tombstone instead fixes the terminal status and preserves its
generation and time. A session that exited is distinct from one explicitly closed.

`reopen` creates a new session after the source is closed; it does not move the source back to
`ready`. A marker in the source admits one reopen. Provider ownership checks and reconciliation
of refused launches remain part of that operation; see [provider support](providers.md).

## Session records

Start with `src/native/session/names.rs` for the shared record names. Some exist only during a
transition. When handling a missing record, check the writer's ordering and interruption tests
before deciding that the operation never happened.

| Record | Evidence or role |
| --- | --- |
| `manifest.json` | Recorded provider, workspace, launch choices, and session identity |
| `cancel.json` | Latest cancel intent: `{schema: 1, request_id, claim_token, created_unix_ms}` |
| `hold.json` | User follow-up hold: `{held: true, created_unix_ms}`; absent means released, unreadable means unknown |
| `status.json` | Current recorded state, generation, time, diagnostic error, and optional residual-surface observation |
| `initial-prompt.txt` | Initial prompt retained for launch/delivery |
| `native-session.json` | Recorded owner identity used for attestation |
| `provider-process.json` | Spawned provider identity, including refused-reopen checks |
| `launch.json` | Launch claim, deadline, and spawn phase |
| `turn.claim` | Current exclusive right to deliver a turn |
| `turn.claim.lock` | Synchronizes claim, completion, exit, and close transitions |
| `status.lock` | Synchronizes status updates |
| `requests/` | Request receipts binding public requests, claims, and event addresses |
| `turn.completion.json` | Accepted completion journal awaiting convergence |
| `events/` | Published turn events and preserved unpublished evidence |
| `terminal.json` | Bound surface handle available to the lifecycle |
| `terminal.closing.json` | Surface handle held by an in-progress close |
| `terminal.closed.json` | Consumed surface handle retained as a tombstone |
| `terminal.close-intent.json` | macOS attested intent for an interrupted explicit close |
| `closed.json` | Terminal session-status tombstone |
| `reopen.marker.json` | Source reservation naming its one reopen |
| `reopen.refusal.json` | New session's recorded launch-phase reopen refusal |

For launch failure details, read `status.json` and `launch.log` alongside `launch.json` and the
claim. The receipt records spawn phase, not a failed or cancelled state.

Compatibility records `resume.pending.json` and `resume.running.json` can occur in older Windows
Claude sessions. New sessions do not create them; close and pruning account for them. The
`unpublished-` prefix marks an event set aside during close because its journal did not verify it.
At the state root, `state-root.durable` records completion of the ancestry durability barrier;
it is not a per-session result.

Adapters own additional names and schemas for hook settings, transport evidence, input prompts,
logs, and surface identity. They access these through the session primitives, but the shared name
list is not a registry of every adapter-private record. [Security and data](security-and-data.md)
describes storage locations, permissions, and retention.

## Turn lifecycle and publication

A request is the caller-facing address of one turn. A claim is its exclusive delivery right;
a receipt binds that request to the claim and the event address chosen before delivery.
Only one claim may exist at a time. Provider-owned correlation decides whether a completion
belongs to it; shared request identity does not replace that correlation.

A proven not-sent delivery can roll back its own claim. A sent or uncertain delivery keeps the
claim until completion, failure, or explicit lifecycle action; uncertainty is never resolved by
blindly repeating delivery. Queue acceptance and terminal-input success are not results.

Under the lifecycle lock, an accepted completion is written as a completion journal. Recovery
validates it, publishes its event, updates status, releases the matching claim, and removes the
journal. Publication requires `created_unix_ms`, validates the journal's identity and status,
and checks the event bytes and size limit. It does not impose a timestamp range or elapsed-time
bound. A damaged receipt does not suppress an independently verified provider completion, but
request lookup remains explicitly unresolved rather than guessing another result.

Readers apply the same publication predicate without completing these writes. Thus a timeline
or result query can report recovery needed without making an unpublished completion into a
result. Timeline entries describe observed records; derived request summaries are identified as
such rather than invented as historical events.

Commands hold a Claim for Delivery and settlement; provider adapters report completion through
`turn::Report`. Completion tests use that same interface, including interrupted publication and
late delivery outcomes. Agy keeps its Result evidence reader inside its adapter: both the monitor
and diagnostics resolve truncated transcript rows and correlate the complete body there, while
only the monitor publishes the Result or records a subsequent failure.

## Record and adapter seams

`session::Reader` opens a read-only view without recovery or mutation. `session::Store` exposes
writes and a reader view. Constructing a Store performs no recovery. `Store::converge` calls dead-owner repair, which recovers accepted completions first and
retains recovery damage in a dead owner's close diagnostic; call it from a mutating command
where needed.
`session::hold` writes and removes `hold.json` under the lifecycle lock using the atomic private
writer and directory-synced removal, without a status write or status lock. Tell's read-only hold
pre-check precedes convergence; admission repeats under the lifecycle lock before a claim and
before delivery. Inspect, status, and doctor read Hold as a separate `SessionEvidence` part while
holding the shared lifecycle lock, with legacy no-lock change detection. It is not a publication
Snapshot input; damaged Hold records do not affect result, wait, search, or context reads.
Timeline does not record hold history. Close preserves the record; prune removes its directory.

Cancel records intent for one Request under the lifecycle lock using the private atomic writer
(and `persist_record` for replacement on Windows). It admits running or working sessions only
with a Claim, its Receipt, no published Result, and session-specific adapter support. The record
survives settlement and later Claims; a new cancel replaces stale intent. Report marks a
provider interruption `cancelled` only with matching cancel evidence, otherwise it is an ordinary
failure. A cancelled event has an error and no successful body; false is omitted to preserve
legacy journal bytes. Cancel never changes the publication predicate or Claim-release barrier.
Readers treat it as auxiliary evidence: malformed intent cannot hide a Result, and timeline
marks its interpretation as derived. Replacement removes the earlier intent from observation.

`RecordReader` and `RecordStore` provide the primitives for names and schemas owned by adapters.
Core Record decoding is not an adapter concern: it lives in `session/record.rs`, one decoder per
Record, and every reader of a core Record calls it.

The native provider contract requires explicit launch configuration, initial delivery,
follow-up transport, completion handling, trust, diagnostics, and reopen behavior from every
provider. There is no default adapter that silently gives all CLIs the same transport. Launch
plans also state which caller environment markers must be removed.

The terminal seam binds a concrete surface to a session and reports whether delivery was known
not to start or may have occurred. `terminal::ownership` checks the recorded owner against the
live process and surface: PID birth, TTY and process groups on macOS, and process identity on
Windows. Terminal.app also records the app incarnation: a window identity in one run of the app
is not authority over the same apparent identity after a restart.

The terminal seam passes the session directory to verification, presence, close (including
owned and dead-owner close). Adapters read their session records
from that directory. Warp requires the handle's managed session id to match the supplied
private directory. macOS guarded input uses the surface handle. Windows guarded input
receives the directory and requires it to match the helper
child's state-root lookup; the child still resolves the session by id under its own root.

Identity stays with each adapter, and each terminal kind selects its close evidence in
`close_evidence::for_kind`: Terminal.app's evidence is its own (app incarnation), and the
presence-only kinds share one adapter whose only declared variation is the ownerless-startup
message context (Warp and WezTerm). Terminal.app keeps `terminal_app` in
the owner record. Warp keeps its control binding in adapter-private `warp-control.json`.
WezTerm keeps `wezterm_mux` in the surface handle. Windows keeps the console root's process
identity in the handle and also uses `console-host-process.json` and owner and launch
evidence for close. iTerm2 and Ghostty use their native ids.

The launcher owns the residual-surface compatibility marker in stored and returned launch
diagnostics; adapters return the typed retained-surface handoff and its cleanup uncertainty.
The recovery close in #87 is not implemented. Its authority decision waits for Warp runtime
evidence from a `warp_control_cli` build or an upstream change.

Close authority selects one of four outcomes: close with an attested owner, close a proven
owned surface, end a live attested owner whose surface is proven absent, or record that the
surface is absent. An ambiguous identity refuses close and
retains the handle. `session::close` records the transition and tombstones, and converges
interrupted close. `terminal::close_authority` makes the decision over its observation seam;
`terminal::ownership` performs the authorized surface/process action. Dead-owner
repair settles the records without resending a prompt or guessing ownership of an unrelated surface.

## Where to add things

- For a new provider, start with `ProviderAdapter` in `src/providers/mod.rs` and
  `NativeProviderAdapter` in `src/native/provider/mod.rs`. Add the CLI identity and flag mappings
  in the former layer, then implement launch, delivery, completion, trust, diagnostics, and
  reopen decisions in the latter. Keep the provider's payloads and tests beside its adapter;
  unrelated adapters should not need changes. Document what upstream feature would let you
  remove each fallback.
- For a new terminal, start with `src/native/terminal/mod.rs` and its surface and delivery types.
  Before implementing close, read `src/native/terminal/close_authority.rs` for the decision and
  the seam, `close_evidence.rs` for selecting the kind's evidence, and `ownership.rs` for the
  owner-teardown action. Add application-native
  operations under the platform module and tests that prove the target belongs to the session.
  Register the kind once: its `facts.rs` declares one `Registration` with the close facts,
  paste envelope and guarded-dialog capability (`SurfaceFacts`), the host command, the
  close-evidence choice, the refusal message for each operation the kind does not support and,
  on macOS, its `BindingFacts` diagnostics and budgets. `TerminalKind::registration` is the one
  exhaustive lookup on every build target; the shared lookups and the shared macOS refusals read
  it, so nothing else needs a per-kind match. For macOS operations, implement the
  `MacTerminalAdapter` methods for the operations the kind supports, behind an injectable
  dependency, and add the adapter to the `Dispatch` in `macos/mod.rs`; an operation the
  registration declares unsupported is refused before the adapter is called. What an adapter
  does for a live owner before ending its surface
  lives in its `close_owned_session`. Reuse `open_with` for binding and cleanup unless the adapter
  owns a distinct open protocol, as Warp does. Keep screen and presence scripts in the adapter.
  A creation preference in settings cannot substitute for recorded close authority.
- For a new platform, start with the dispatch in `src/native/terminal/mod.rs` and the refusal
  paths in `src/native/terminal/linux/mod.rs`. Then read the process-identity code under the
  existing platform directories, private-record permissions in
  `src/native/session/primitives.rs`, and trust-store checks in `src/native/consent.rs`. Implement
  those checks along with terminal operations before claiming support. Linux currently refuses
  visible-session operations; compilation and fixtures do not establish a live round trip.

For a lifecycle race or partial transition, first add a deterministic test that reproduces it,
then change the implementation. Keep observation read-only and prefer the provider's own
mechanisms over extending a fallback. Follow [AGENTS.md](../AGENTS.md) for regression requirements
and [docs/testing.md](testing.md) to record authenticated terminal checks; its section "Simulated
time and the race hooks" names the one `Clock` seam, the deterministic hooks, and why an I/O
context on `Store` and `Reader` was declined.
