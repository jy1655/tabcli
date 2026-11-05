# macOS Node spawnSync stdin stall, 2026-11-05

`tests/claude_completion.rs` hung on GitHub's macOS runners and printed nothing until the run
was cancelled; a re-run passed each time. This record holds the CI observations, a local
reproduction that involves no `tabcli`, the test-harness change, and what remains unknown.
No product code changed: the real completion module calls `$.process.run` with its own
timeout and one retry (`src/native/provider/claude/report.js`).

## CI observations

| Run, attempt 1 | Runner image | Test started (UTC) | Cancelled | Terminated by the runner |
| --- | --- | --- | --- | --- |
| [40917217754](https://github.com/jy1655/tabcli/actions/runs/40917217754) | `macos-26-arm64` 20261013.0424.1 | 2026-10-29 17:08:15 | 18:09:48 | `tabcli`, `node`, test binary |
| [41016053245](https://github.com/jy1655/tabcli/actions/runs/41016053245) | `macos-26-arm64` 20261021.0445.8 | 2026-10-30 06:53:55 | 07:03:56 | `tabcli`, `node` |

Both were `rust (macos-latest)` jobs of CI pushes to `main` on macOS 26.6.2 (25G83). In each
log the last test output is `running 1 test` for `tests/claude_completion.rs`, which prints
nothing until it finishes. The runner's orphan cleanup then terminated one `tabcli` and one
`node` process. No stack, sample or per-call record was captured, and the job logs do not name
the runner's Node version.
Other attempts cancelled in the same period, such as runs 41873303549 and 40913463578, recorded
no steps and are not counted here.

Before this change, the harness called `tabcli` through `spawnSync(executable, args, { input, ... })` with no
timeout, and the Rust test waited for Node with `Command::output()`, also unbounded.

## Local reproduction without tabcli

Environment: macOS 26.5.2 (25F84), Apple M2 Pro, Node v24.12.0. Each script spawns
`/usr/bin/wc -c` in a loop and checks that it counted every byte. The first `input` run used
the same loop without the heartbeat and a fixed 4 MiB payload.

```js
// spawnsync-stdin2.mjs <log> <heartbeat> <iterations> <bytes>
import { spawnSync } from "node:child_process";
import { appendFileSync, writeFileSync } from "node:fs";
const [out, hb, totalArg, sizeArg] = process.argv.slice(2);
const total = Number(totalArg), size = Number(sizeArg);
const input = Buffer.alloc(size, "a");
let max = 0, timeouts = 0, errors = 0;
const started = Date.now();
writeFileSync(hb, `${process.pid} 0 ${Date.now()}\n`);
for (let i = 0; i < total; i++) {
  if (i % 20 === 0) writeFileSync(hb, `${process.pid} ${i} ${Date.now()}\n`);
  const t = Date.now();
  const r = spawnSync("/usr/bin/wc", ["-c"], { input, timeout: 30000, killSignal: "SIGKILL" });
  const ms = Date.now() - t;
  if (ms > max) max = ms;
  if (r.error?.code === "ETIMEDOUT") { timeouts++; appendFileSync(out, `TIMEOUT i=${i} ms=${ms} size=${size}\n`); }
  else if (r.error || r.status !== 0 || String(r.stdout).trim() !== String(size)) {
    errors++; appendFileSync(out, `ERROR i=${i} ms=${ms} status=${r.status} err=${r.error?.code}\n`);
  } else if (ms > 1000) appendFileSync(out, `SLOW i=${i} ms=${ms}\n`);
}
appendFileSync(out, `DONE size=${size} total=${total} max_ms=${max} timeouts=${timeouts} errors=${errors} elapsed_s=${Math.round((Date.now()-started)/1000)} node=${process.version}\n`);
writeFileSync(hb, `done\n`);
```

```js
// spawnsync-filefd.mjs <log> <iterations> <bytes> <payload file>
import { spawnSync } from "node:child_process";
import { appendFileSync, writeFileSync, openSync, closeSync } from "node:fs";
const [out, total, size, payload] = [process.argv[2], Number(process.argv[3]), Number(process.argv[4]), process.argv[5]];
writeFileSync(payload, Buffer.alloc(size, "a"));
let max = 0, timeouts = 0, errors = 0;
const started = Date.now();
for (let i = 0; i < total; i++) {
  const fd = openSync(payload, "r");
  const t = Date.now();
  const r = spawnSync("/usr/bin/wc", ["-c"], { stdio: [fd, "pipe", "pipe"], timeout: 30000, killSignal: "SIGKILL" });
  const ms = Date.now() - t;
  closeSync(fd);
  if (ms > max) max = ms;
  if (r.error?.code === "ETIMEDOUT") { timeouts++; appendFileSync(out, `TIMEOUT i=${i} ms=${ms}\n`); }
  else if (r.error || r.status !== 0 || String(r.stdout).trim() !== String(size)) {
    errors++; appendFileSync(out, `ERROR i=${i} ms=${ms} status=${r.status} err=${r.error?.code}\n`);
  }
}
appendFileSync(out, `DONE filefd size=${size} total=${total} max_ms=${max} timeouts=${timeouts} errors=${errors} elapsed_s=${Math.round((Date.now()-started)/1000)} node=${process.version}\n`);
```

A watchdog (`watchdog.sh <heartbeat> <directory>`) sampled both processes whenever the
heartbeat was more than 8 s old:

```sh
#!/bin/sh
# Samples the node harness and its wc child when the heartbeat goes stale.
hb="$1"; dir="$2"; n=0
while :; do
  sleep 2
  line=$(cat "$hb" 2>/dev/null) || continue
  [ "$line" = "done" ] && exit 0
  pid=$(echo "$line" | cut -d' ' -f1); ts=$(echo "$line" | cut -d' ' -f3)
  now=$(python3 -c 'import time;print(int(time.time()*1000))')
  age=$((now - ts))
  if [ "$age" -gt 8000 ] && [ ! -f "$dir/sampled-$ts" ]; then
    touch "$dir/sampled-$ts"; n=$((n+1))
    child=$(pgrep -P "$pid" -x wc | head -1)
    echo "stall age_ms=$age node=$pid child=$child line=$line" >> "$dir/stalls.log"
    /usr/bin/sample "$pid" 2 -file "$dir/sample-node-$ts.txt" >/dev/null 2>&1
    [ -n "$child" ] && /usr/bin/sample "$child" 2 -file "$dir/sample-wc-$ts.txt" >/dev/null 2>&1
    [ -n "$child" ] && lsof -p "$child" > "$dir/lsof-wc-$ts.txt" 2>&1
    lsof -p "$pid" > "$dir/lsof-node-$ts.txt" 2>&1
  fi
done
```

| How stdin is passed | Payload | Iterations | Stalls (killed at 30 s) |
| --- | ---: | ---: | ---: |
| `input` option | 4 MiB | 80,000 (20,000 + 60,000) | 3 |
| `input` option | 138,240 B | 150,000 | 21 |
| File descriptor, `stdio: [fd, "pipe", "pipe"]` | 138,240 B | 150,000 | 0 (longest call 12 ms) |

Every stall ended when `spawnSync`'s own 30 s timeout killed the child (29,980 to 30,006 ms);
the loop then continued, and no other error occurred. In the heartbeat runs no other call
took over 1 s. In two sampled stalls Node's main thread
was in `SyncProcessRunner` → `uv_run` → `uv__io_poll` → `kevent`, and the `wc` child was in
`read` on fd 0, a Unix socket whose peer was Node. Neither process used CPU. The sample files
are not retained with this record.

## Established and not established

Established:

- On this Mac, Node v24.12.0's `spawnSync` can stall while it writes `input` to the child's
  stdin, with neither process using CPU, at both payload sizes. `tabcli` is not involved.
- `spawnSync`'s `timeout` option ended every stall, so a per-call timeout bounds it.
- The same 150,000 transfers through a file descriptor did not stall.
- Both CI hangs occurred inside this test, with one `node` and one `tabcli` process alive.

Not established:

- That the CI hangs were this stall: no stack was captured on a runner.
- Whether the cause lies in Node and libuv or in the macOS kernel.
- Whether an upstream Node or libuv report exists; none was searched for or filed.
- A file-descriptor run at 4 MiB, or an `input` run with payloads under 512 bytes.
- A matching rate. Each test run passes 72 payloads: 30 of 135,264 to 135,436 bytes, one of
  4,194,304 bytes and 41 under 512 bytes; 23 more calls have no stdin. At the local
  138,240-byte rate (21 in 150,000), the 31 large transfers predict a stall in roughly 0.4% of
  runs. The two CI hangs fell among 86 CI and Release attempts created from 2026-10-29 to
  2026-11-05, each with at most one macOS test step: about 2%. Hosted runners differ from this
  Mac, so the two rates are not directly comparable.
- Windows and Linux behavior of the file-descriptor path, which only their CI runs show.

## Test-harness change

`src/native/provider/claude/completion_test.mjs` sends every `tabcli` call through one helper.
A payload is written to a file and passed as a read-only file descriptor; calls without stdin
get `ignore`. Every call has a 60 s timeout with `SIGKILL`: the slowest healthy call is `wait
--timeout-secs 1` at about 1.0 s, and the product allows the same helper 30 s. Each call
appends a start line and an end line (scenario, call number, arguments, stdin bytes, exit
status, signal and elapsed time) to `progress.log` in a per-scenario directory outside the
state root and the session directory: the `coexistence` scenario requires `result` to leave
the session directory byte-identical. A timeout or spawn error writes a diagnosis to stderr
with `fs.writeSync` and exits with code 70. Throwing would not do: `report.js` catches the
error and retries. The scenarios that simulate a failing helper (`helper_failure`,
`retry_before`, `retry_after`, `continuation_retry_after`, `questions_retry`) return or throw
before or after the real call, as before; arguments, environment, output handling and the
emulated `$.process.run` result are unchanged.

`tests/claude_completion.rs` spawns each scenario's Node process with its output in a file,
polls it, and kills it after 300 s. A failed or killed scenario's panic names the scenario and
includes Node's output and the progress log. The CI and Release serial test steps now have
`timeout-minutes` bounds (see `docs/testing.md`).

## Verification

On the Mac above, the test binary built from this change (SHA-256
`2e29b3c34c515659ad98a74f846c380ef67b883c848d37d9e7901672cccf4d55`, with `tabcli`
`ee4de65159173e5c6be6a1f046b175ed78461831938796b59cdf51eb876bf716`) ran 40 times in a row:
40 passed, in 9.28 to 10.05 s each (median 9.47 s). Each run makes 95 `tabcli` calls: 72
`native-hook` calls with stdin, 10 `result`, 9 `wait` and 4 `inspect`. In one run whose
progress logs were kept, the slowest call was a `wait --timeout-secs 1` at 1,018 ms. These runs
show that the changed harness works; they cannot show that the stall is gone. Their 1,240 large
transfers would expect about 0.2 stalls at the local `input` rate.

Each failure path was exercised once with a temporary edit that was then reverted. With a 1 ms
call timeout, the harness exited with code 70 and the panic named scenario `unicode`, call 1,
`["native-hook","claude"]`, 117 stdin bytes, `SIGKILL` and `ETIMEDOUT`. A missing executable
gave `ENOENT` and exit code 70. With a 200 ms scenario limit, the runner killed Node, and the
progress log showed the call in flight as a start line without an end line.

Gates after a rebuild: `cargo fmt -- --check`; all-target, all-feature Clippy with
`-D warnings` on macOS and for `x86_64-pc-windows-msvc`; the full serial suite with 1,613
passed, none failed and six ignored; `git diff --check`; the language lint; and the Python 3.13
script tests (109 run, two skipped). Ruby's YAML loader parsed both workflows; no workflow
linter was run. Windows and Linux runs of this change, including the file-descriptor stdin
and the step timeouts, are first observed in CI.
