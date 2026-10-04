# Codex queue: remove the shared-daemon prerequisite

2026-10-04, native Windows, Codex CLI 0.160.0, Rust 1.97.1. Repository base:
`2c86a34`. This records a local source change, not an installed or published release.

## Finding and change

Codex 0.157.0 enabled automatic background-server startup for eligible interactive
sessions. In 0.160.0, the daemon startup policy still excludes Bridge's `-c notify=...`
override. That does not make a shared daemon necessary for the queue command:
`codex queue` can connect to the shared server or start an embedded server. Both
write Codex's durable queue, and the process holding the target thread watches
external changes. The queue command does not run the TUI's automatic-daemon-start
path.

Bridge used to run `app-server daemon version` before queueing and refuse a valid
embedded queue path when that probe failed. The Codex adapter now leaves backend
selection to the official queue command. It keeps the minimum queue version,
authoritative thread UUID, exact acceptance confirmation, claim, bounded process
tree, and correlated completion checks. It never substitutes terminal input or
retries an uncertain delivery. No other provider adapter or shared lifecycle code
changed. `doctor --probe` retains its read-only daemon observation and describes
it as advisory.

Source evidence:

- [0.157.0 release entry](https://learn.chatgpt.com/docs/changelog): automatic
  startup applies to eligible interactive sessions, not every CLI invocation.
- Codex 0.160.0, commit `a956835d020762cb2b570053af06f643a11c0ecc`:
  [daemon exclusions](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/tui/src/daemon_startup.rs),
  [queue backend selection](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/tui/src/session_archive_commands.rs),
  [queue command](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/tui/src/session_queue_commands.rs),
  [external queue watcher](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/ext/queue/src/service.rs).
- The existing queue minimum, 0.149.0, already has both the
  [embedded sender](https://github.com/openai/codex/blob/758ef40f50c1a458425c7cfbf1eb12cbc07af0b0/codex-rs/tui/src/session_queue_commands.rs)
  and [external queue watcher](https://github.com/openai/codex/blob/758ef40f50c1a458425c7cfbf1eb12cbc07af0b0/codex-rs/ext/queue/src/service.rs).
  This is source evidence for that version, not a 0.149.0 runtime test.

## Native CLI protocol probe

The installed 0.160.0 executable ran in a fresh, separate `CODEX_HOME`. A local
HTTP fixture returned fixed Responses events; no account credentials or actual
model service were used. No shared daemon was installed or started in that home.
The user's existing daemon was not stopped, restarted, or reconfigured.

1. A foreground stdio app-server created two real threads, A and B, and completed
   one fixture turn in each so that both conversations were persisted.
2. A separate `codex queue --thread A --message QUEUE_PROBE_INPUT` exited 0 and
   reported an accepted item for A.
3. The foreground server consumed that item at its external queue poll, roughly
   10 seconds later. Only A started a new turn; it completed with `QUEUE_PROBE_OK`.
4. The actual `notify` payload named A, a new turn ID, and input messages ending
   with `QUEUE_PROBE_INPUT`. B received no queued turn.
5. The foreground server and local HTTP fixture were shut down after observation.

Raw evidence is in `D:\Dev\ab-probe-fu8qmh97\`: `summary.json`,
`app-server.jsonl`, `notify.jsonl`, and `model-requests.jsonl`.
Target A is `01a1051b-769f-7cb2-add1-62ec988d3296`; control B is
`01a1051b-76ae-7e60-aa38-14db5c4cd602`; the queued turn is
`01a1051b-9daf-7633-af75-b51898bdc6cb`.

The daemon probe failed both before and after. A failed probe alone is not proof
of absence: the isolated home and the processes explicitly started for this
fixture establish the test arrangement. An earlier setup attempt used a home
whose socket path exceeded the Windows limit and queued before any initial turn
had persisted; it was rejected with `no rollout found`. That attempt is not
counted as a delivery pass.

This proves native cross-process queue acceptance, consumption, and notification
with a mock model. It does not prove an authenticated TUI round trip, agent-picker
interaction, macOS runtime, or terminal cleanup. It does not close issue #57 or #65.

## Repository regression

The adapter test uses a provider that rejects a daemon probe but accepts the exact
UUID-addressed queue command. Before the change it failed at the daemon gate.
After the change it passes on native Windows and verifies that no daemon probe
runs, the pending claim remains until completion, and acceptance publishes no
new result. The fixture has Windows and Unix implementations. Existing tests
continue to cover wrong-thread confirmations, unsupported queue responses,
uncertain delivery, and completion correlation.

The targeted Windows Codex adapter suite passed: 25 tests, 0 failures.

Final repository checks on native Windows:

- `cargo test --all-targets --all-features -- --test-threads=1`: 540 passed,
  0 failed, 5 ignored. Ignored tests are not runtime verification.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo fmt -- --check`: passed.
- `git diff --check`: passed.

The full test log is in
`C:\Users\user\AppData\Local\Temp\agent-bridge-codex-daemon-b732cd8ac41c45ebb2dd6c59f46cc88c\cargo-test.log`.
