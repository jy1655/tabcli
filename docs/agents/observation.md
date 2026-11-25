# Observation and Self-Test Boundary

- A query observes and nothing else: `inspect`, `inspect --timeline`, `result`, `search`
  and the read-only form of the sessions query never repair, recover, resend, close or
  write. A new query reuses the readers and the publication rules of the existing ones,
  so that the same request has the same state everywhere.
- The timeline shows what the records hold. It reads every record once for one answer,
  strictly as UTF-8 and within a bound, reports a record it could not read as such, and
  fails when a record changed while it read. What it derives is marked: the request
  summaries are what `result` reports at the time of the query and carry `derived_from`.
  Do not add an entry for something no record holds.
- Doctor uses the retained launch host diagnosis. For a bound Linux or Windows Herdr host,
  its `launch_stage` row keeps `launch_host_failed` and directs the caller to the launch
  log for an input-flush or shell-start error. The shell-environment probe guidance applies
  to the macOS capture host.
- `screen` observes the bound surface only. It reads the surface record under the Snapshot's
  shared lock and releases the lock; a kind's declared screen-read refusal then answers before
  any adapter, `tell`'s surface ownership check runs, one read goes through the shared
  dispatch, and the text is withheld unless a fresh binding read is the same handle and the
  check passes again for an owner record identical to the one it first verified. That shows one
  owner incarnation at both checks, not throughout the read. It sends no input, writes the text
  to no record, log or diagnostic, addresses no terminal application that is not running when
  it checks (one that quits before the read can be started again, as with the presence and
  close scripts), and claims nothing about the request.
- `created_unix_ms` of a receipt and of an event is optional only when a record is read,
  so that a damaged record can be reported with a reason. Every writer writes it, and no
  path may publish an event without it: the completion-journal validation rejects one.
- The elapsed time is what TAB observed between two records. Do not present it as
  model time or billing time, and do not collect provider usage from a signal that the
  provider does not document as usage. Usage a query shows is what the event records
  (`usage`, or `usage_reason` with the bounded `usage_error`); never estimate or total it.
- A delivery refusal (`Delivery::NotSent`) is settled by `settle_delivery_refusal_locked`
  as the receipt's failed event: TAB-owned evidence of what was not sent, with
  `provenance: "delivery_not_sent"`, the refusal text as `error`, the session's provider
  from the manifest and no provider session or turn id. It is journaled through the same
  completion machinery under the one lifecycle lock, after a pending completion is
  recovered and only for the installed claim of a session that is not closed; a failed
  journal write leaves the claim installed. Every query reads it as `failed`; the timeline
  marks it `stage: delivery_refusal` and `delivery: refused_not_sent`; reopen accepts it.
  Historical receipts without an event stay `unresolved`: never infer a refusal from
  `status.error`, never repair such a receipt, and the reopened-conversation refusal in
  `reopen.rs` still leaves one on purpose.
- An initial prompt that was not delivered can carry a version note: the provider version
  of the failing session beside that of the newest other session of the same provider that
  completed a request on the state root, when the two differ. It is a read of manifests
  and events at failure time, written to no record and added to the returned error only.
  It states the two recorded versions and never that the update caused the failure. The
  start's own error must not wait on it: the scan runs on its own thread, the caller waits
  at most 5 s and abandons a scan that has not answered, and the scan is bounded (20,000
  entries of the state root, 64 KiB a manifest, the 64 newest sessions of the provider,
  1,024 events a session, 16 MiB of events read as strict UTF-8). A spent bound, a
  manifest or event it cannot read or decode, an `events` that is not a real directory or
  an event name that is not a regular file ends it without a note, because the unread
  session could be the newest one. The same version adds nothing.
- The self-test runs in the ordinary state root by default and in a private directory
  only with `--isolated` (owner's decision, 2026-10-02): its purpose is to check the
  installed setup as it is. It closes only what it created, bounds every command it
  runs, never resends after an uncertain delivery, and never deletes a record. It
  follows `ask`'s workspace-consent rules (owner's decision, 2026-10-06): verified
  consent for the exact workspace is reused through the provider's own approval, as
  in `ask`, and without it the self-test approves no workspace trust itself. An
  explicit `--yolo` only forwards the provider's own bypass option. `--isolated`
  separates TAB's records, not a provider's trust store. Nothing else may call it.

