# Architecture

Follow an `ask` from launch preparation to a retained result to find the module you need to change.
The Rust CLI manages one provider conversation per session in a visible surface. A request crosses
these boundaries:

```text
native.rs: parse ask, check inputs, prepare the session directory
  -> session::turn + session::requests: acquire the initial claim and write its receipt
  -> provider + terminal: prepare the CLI and bind a newly created surface
  -> session::launch: record provider spawn or launch failure
  -> provider: deliver the prompt and recognize the matching completion
  -> session::turn: write the completion journal
  -> session::turn: publish the result, update status, release the claim, remove the journal

result / inspect
  -> query: read the published result without changing records

close-session
  -> session::close: claim the surface handle for close
  -> terminal::ownership: decide close authority and act on the owned surface
  -> session::close: settle the close records and retain the tombstones
```

A follow-up enters through `tell` in `native.rs` and claims the existing session before delivery.
The admission checks read Hold without taking another lock; `claim_ready` identifies follow-up
claims so the final `begin_delivery` hold gate excludes initial delivery.
It uses the same completion journal and publication path. If delivery is uncertain, the claim
stays held; neither a query nor repair resends the prompt.

A record is durable evidence of an operation, not a disposable cache. Use the terms in
[CONTEXT.md](../CONTEXT.md) when reading the lifecycle code. The rules for changes live in
[AGENTS.md](../AGENTS.md).

## Module map and dependencies

- `src/main.rs` contains top-level dispatch and help; `src/native.rs` parses native commands,
  coordinates launch and delivery, and defines the shared manifest and event types.
- `src/native/reopen.rs` owns Reopen source reservations, provenance, launch refusals,
  and their settlement, including read-only marker diagnostics. It creates the new Session
  and records its provenance before finalizing the source marker. Launch and delivery keep
  their existing holder-check timing; provider adapters supply the holder evidence.
- `src/providers/` defines the CLI identities, minimum versions, and model, effort, and bypass
  argument mappings. The library uses these mappings before the native launch path.
- `src/native/provider/` owns each provider's launch and resume plans, trust interpretation,
  transports, completion correlation, hooks, diagnostics, and adapter-private records. It uses
  the session seam and terminal operations. Keep provider schemas and transport selection in
  `src/native/provider/<provider>.rs`, not in shared orchestration.
- `src/native/terminal/` selects and implements platform-specific surfaces, input, observation,
  and close. Its `ownership` module owns owner identity, attestation, app incarnation, close
  authority, and the execution of an authorized close.
- `src/native/session/` owns record access and durability. `requests` binds public requests to
  claims and events; `turn` owns delivery settlement, completion and recovery; `close` owns
  close-record transitions and dead-owner repair; `launch` coordinates spawn, cancellation,
  and the full macOS binding wait, with surface identity checks supplied by terminal adapters.
  `state` owns the serialized status and its writes: `Store::update_status` applies
  permitted transitions and generations, while `Store::record_residual_surface` amends
  diagnostics without changing lifecycle fields. Both preserve closed tombstones.
- Within `src/native/session/`, start with `reads.rs` for decoding and read budgets,
  `primitives.rs` for filesystem durability and private-record permissions, and `owner.rs` for
  read-only observations of the recorded owner.
- `src/native/query.rs` and `query/timeline.rs` assemble read-only inspection, result, search, and
  timeline views using the session readers and common publication rules. Queries get a `Reader`,
  not a `Store`. To expose an adapter-private fact, add an observation to that adapter's interface;
  do not read its private record from the query or call repair.
  Request observation precedes JSON rendering. Context attachment resolution uses the same
  observation and requires strict event decoding, an exact event name, and verifiable provenance;
  it does not reconstruct Request meaning from command JSON. Timeline retains its cached strict
  reads, and search retains its cumulative byte budget.
  Inspect, doctor, and `status` render one Observation per session from these readers and publication rules;
  owner process observations and surface probes are added on request, while `sessions` keeps
  its own lock-free listing after its optional repair.
- `src/native/doctor.rs` combines read-only shared observations with provider diagnostics and
  optional bounded probes. Providers choose which observation can explain a result timeout;
  `doctor` assembles that read and `self_test` bounds its execution. A diagnostic report is not
  proof of successful delivery. The report states its scope using the check inputs and existing
  observations.
- `src/native/self_test.rs` runs the explicit public self-test through bounded commands, checks
  initial and follow-up results, and closes only its own sessions. It uses the ordinary state
  root unless isolation is requested; it is not called implicitly by another command.
- `src/native/consent.rs` reads exact-workspace trust evidence, maintains Bridge consent, and
  coordinates its application through provider and terminal adapters. Provider stores are read
  directly but never written directly by Bridge.
- `src/native/settings.rs` owns user preferences for surface creation. A preference cannot grant
  close authority over an existing surface.

## Session lifecycle

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

The native provider contract requires explicit launch configuration, initial delivery,
follow-up transport, completion handling, trust, diagnostics, and reopen behavior from every
provider. There is no default adapter that silently gives all CLIs the same transport. Launch
plans also state which caller environment markers must be removed.

The terminal seam binds a concrete surface to a session and reports whether delivery was known
not to start or may have occurred. `terminal::ownership` checks the recorded owner against the
live process and surface: PID birth, TTY and process groups on macOS, and process identity on
Windows. Terminal.app also records the app incarnation: a window identity in one run of the app
is not authority over the same apparent identity after a restart.

Close authority selects one of four outcomes: close with an attested owner, close a proven
owned surface, end a live attested owner whose surface is proven absent, or record that the
surface is absent. An ambiguous identity refuses close and
retains the handle. `session::close` records the transition and tombstones, and converges
interrupted close; `terminal::ownership` performs the authorized surface/process action. Dead-owner
repair settles the records without resending a prompt or guessing ownership of an unrelated surface.

## Where to add things

- For a new provider, start with `ProviderAdapter` in `src/providers/mod.rs` and
  `NativeProviderAdapter` in `src/native/provider/mod.rs`. Add the CLI identity and flag mappings
  in the former layer, then implement launch, delivery, completion, trust, diagnostics, and
  reopen decisions in the latter. Keep the provider's payloads and tests beside its adapter;
  unrelated adapters should not need changes. Document what upstream feature would let you
  remove each fallback.
- For a new terminal, start with `src/native/terminal/mod.rs` and its surface and delivery types.
  Read `src/native/terminal/ownership.rs` before implementing close. Add application-native
  operations under the platform module and tests that prove the target belongs to the session.
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
and [docs/testing.md](testing.md) to record authenticated terminal checks.
