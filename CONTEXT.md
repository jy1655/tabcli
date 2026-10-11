# Terminal Agent Bridge (TAB)

Agent Bridge launches, observes, continues, and closes first-party CLI sessions (Codex, Claude,
Agy, Pi) inside terminal surfaces the user can see. This glossary names the concepts the code
and its documents share; `AGENTS.md` holds the decisions about them.

## Language

### Sessions and their records

**Session**:
One managed provider conversation in one terminal surface, identified by a session id and
owned by Bridge from launch to close.
_Avoid_: child, tab, process

**Session directory**:
The private directory that holds every record of one session.
_Avoid_: state dir, workdir

**Record**:
One durable fact about a session that Bridge wrote and later reads back (status, manifest,
owner, claim, receipt, event, journal, tombstone). A record is evidence, never a cache.
_Avoid_: file, state file, metadata

**Reader**:
The read-only view of a session's records. A query holds a Reader and can therefore observe
and nothing else.
_Avoid_: snapshot, inspector

**Observation**:
The read-only account of one session that a Reader yields: its recorded lifecycle, the
active request and its state, its published results, and what those records imply. A query
renders it rather than deriving its own; owner and surface facts are added to it on request.
_Avoid_: view, summary, status report, listing row, snapshot (an internal read, not the account)

**Store**:
The read-write view of a session's records, held by a command that changes a session (ask,
tell, hook, close, repair). A Store converges unfinished work before it is used.
_Avoid_: state, repository, manager

**Adapter-private record**:
A record whose name and schema one provider or terminal adapter owns. It lives in the session
directory and is written with the Store's primitives, but no other module reads it.
_Avoid_: scratch file, side file

### Turns

**Turn**:
One prompt delivered to the provider and the provider's one answer to it.
_Avoid_: message, round, exchange

**Claim**:
The exclusive right to deliver the next turn of a session. One claim exists at a time; it is
released when the turn completes, fails, or is rolled back.
_Avoid_: lock, lease, token (the token is the claim's identifier, not the claim)

**Request**:
The caller-facing identity of one turn (`request-…`), fixed before delivery and never reused.
_Avoid_: job, task, message id

**Receipt**:
The record that binds a request to its claim and, once published, to its result event.
_Avoid_: mapping, index entry

**Completion journal**:
The record of a provider completion that Bridge accepted but has not yet published as an
event. Recovery publishes or discards it.
_Avoid_: pending result, buffer

**Result**:
The published event of a completed turn, as `result` reports it.
_Avoid_: output, response, answer

**Delivery**:
The act of putting a prompt in front of the provider. Its outcome is sent, not sent, or
uncertain; an uncertain delivery is never retried blindly.
_Avoid_: send, submit, paste (these name transports, not the act)

**Initial prompt**:
The first turn's prompt, delivered as part of launching the session.

**Follow-up**:
Any later turn's prompt, delivered with `tell` into the running session.
_Avoid_: continuation, reply

**Hold**:
The user's recorded request that a session take no further follow-up: a follow-up whose delivery
Bridge has not yet allowed to start is refused until the hold is released. A hold does not stop
the current turn or direct input to the surface.
_Avoid_: pause, freeze, mute, lock (the lock is a different thing)

**Cancel**:
The user's recorded request to interrupt the current turn instead of waiting for its response.
It belongs to that turn's Request, which ends as cancelled only when the provider's own signal,
correlated to that turn, confirms interruption. Cancel does not close the session, change Hold,
or deliver a follow-up. Requested means the intent is recorded, accepted means the provider
integration handled the call, and cancelled means a correlated provider interruption was
published as a Result.
_Avoid_: stop, kill; abort and interrupt are provider API names only.

### Surfaces and ownership

**Surface**:
The terminal tab, window, pane, or console in which a session's provider runs.
_Avoid_: terminal (the application), screen

**Owner**:
The Bridge process that launched a session and is attested as controlling its surface.
_Avoid_: parent, launcher process

**Residual surface**:
A surface left by a failed launch whose cleanup has not been confirmed. Recording it does
not grant close authority; a closed session can still have a residual surface.
_Avoid_: orphan (absence of an owner does not prove this observation)

**Attestation**:
The proof that a recorded owner is the live process it names: its PID, birth, controlling
TTY and process groups (on Windows its process identity) match what the system reports now.
_Avoid_: liveness check, validation

**App incarnation**:
One run of a terminal application, identified by its PID and birth. A Terminal.app window id
and tty are identities only inside the incarnation that created them.
_Avoid_: app instance, app process

**Close**:
The explicit end of a session: its surface is ended and a tombstone is recorded.
_Avoid_: kill, teardown, cleanup

**Close authority**:
What a close may do to a surface, decided from the session's records and what is observed
now: act with a live attested owner, act on the surface alone, end a live attested owner whose
surface is proven absent, or do nothing because the surface is proven absent. Anything else
refuses the close and keeps the surface handle.
_Avoid_: permission, ownership check

**Repair**:
Converging a session whose owner has died or whose close was interrupted to a consistent
recorded state, without resending anything.
_Avoid_: recovery (reserved for the completion journal), fix-up

**Reopen**:
Starting a new session that continues the provider conversation of a closed session. The
source remains closed; the new session has its own identity and surface.
_Avoid_: restart, unclose

**Reopen marker**:
The record in a closed source session that identifies the reopen holding or consuming it.
_Avoid_: resume lock, continuation token

### Trust

**Workspace trust**:
A provider's own recorded consent to work in an exact workspace, read from a store only the
user can change.
_Avoid_: approval, permission (Bridge's own permission mode is a different axis)
