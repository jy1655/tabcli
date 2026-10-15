// Executes the installed extension with real record I/O and deterministic lifecycle/timers.
import assert from "node:assert/strict";
import * as fs from "node:fs";
import { join } from "node:path";
import vm from "node:vm";

const directory = process.argv[2];
const source = fs.readFileSync(join(directory, "extension.js"), "utf8")
  .replace(/^import .*;\n/gm, "")
  .replace("export default function (pi)", "function install(pi)");

const inputs = JSON.parse(fs.readFileSync(join(directory, "inputs.json"), "utf8"));
const fixtures = [];

async function fixture(name) {
  const dir = join(directory, name);
  const records = inputs[name];
  const first = records[0].token;
  const second = records[1]?.token;
  const record = (token) => records.find((r) => r.token === token);
  const handlers = new Map();
  const timers = new Map();
  const payloads = [];
  let nextTimer = 0;
  let aborts = 0;
  let acceptedWrites = 0;
  let failAccepted = false;
  let throwAbort = false;
  const claim = (token) => fs.writeFileSync(join(dir, "turn.claim"), record(token).claim);
  const cancel = (token) => fs.writeFileSync(join(dir, "cancel.json"), record(token).cancel);
  const context = vm.createContext({
    process: { env: { AGENT_BRIDGE_NATIVE_SESSION_DIR: dir, AGENT_BRIDGE_EXECUTABLE: "fake" }, pid: 1 },
    join, readFileSync: fs.readFileSync, unlinkSync: fs.unlinkSync,
    writeFileSync: fs.writeFileSync,
    renameSync(from, to) {
      if (to.endsWith("pi-cancel-accepted.json")) {
        if (failAccepted) throw new Error("accepted write failed");
        acceptedWrites += 1;
      }
      fs.renameSync(from, to);
    },
    spawnSync(_executable, _args, options) {
      payloads.push(JSON.parse(options.input));
      return { status: 0 };
    },
    setInterval(callback, delay) {
      assert.equal(delay, 200);
      timers.set(++nextTimer, callback);
      return nextTimer;
    },
    clearInterval(id) { timers.delete(id); },
  });
  vm.runInContext(source, context);
  context.install({ on(name, callback) { handlers.set(name, callback); } });
  assert.equal(timers.size, 0, "factory must not start timers");
  const ctx = {
    abort() { aborts += 1; if (throwAbort) throw new Error("stale ctx"); },
    sessionManager: { getSessionId: () => `pi-${name}`, getLeafId: () => `leaf-${name}` },
    ui: { notify() {} },
  };
  const emit = async (name, event = {}) => handlers.get(name)?.(event, ctx);
  const start = async (token, correlated = true) => {
    claim(token);
    await emit("before_agent_start", { prompt: correlated ? record(token).prompt : "manual" });
    await emit("agent_start");
  };
  const tick = () => { for (const callback of [...timers.values()]) callback(); };
  const end = async (reason = "aborted", text = "result") => {
    await emit("agent_end", { messages: [{ role: "assistant", stopReason: reason, content: [{ type: "text", text }] }] });
    assert.equal(timers.size, 0, "agent_end clears timer");
    await emit("agent_settled");
  };
  fixtures.push({ name, payloads });
  return { dir, first, second, records, claim, cancel, emit, start, tick, end, timers, payloads,
    counts: () => [aborts, acceptedWrites],
    failAccepted: () => { failAccepted = true; },
    throwAbort: () => { throwAbort = true; },
  };
}

if (process.argv[3] === "startup") {
  for (const name of Object.keys(inputs)) {
    const f = await fixture(name);
    await f.emit("session_start", { reason: "startup" });
  }
} else {

{
  const f = await fixture("matched_duplicate");
  f.claim(f.first);
  await f.emit("session_start", { reason: "startup" });
  await f.start(f.first);
  f.cancel(f.first); f.tick();
  f.cancel(f.first); f.tick();
  assert.deepEqual(f.counts(), [1, 1]);
  assert.equal(JSON.parse(fs.readFileSync(join(f.dir, "pi-startup-ready.json"))).schema, 2);
  assert.equal(JSON.parse(fs.readFileSync(join(f.dir, "pi-cancel-accepted.json"))).claim_token, f.first);
  await f.end();
  assert.equal(f.payloads[0].agent_bridge_cancel_claim_token, f.first);
}
for (const kind of ["prompt_mismatch", "claim_mismatch", "claim_replaced"]) {
  const f = await fixture(kind);
  await f.start(f.first, kind !== "prompt_mismatch");
  f.cancel(kind === "claim_mismatch" ? f.second : f.first);
  if (kind === "claim_replaced") f.claim(f.second);
  f.tick();
  assert.deepEqual(f.counts(), [0, 0]);
  if (kind === "prompt_mismatch") {
    await f.end();
    assert.equal(f.payloads.length, 0);
  }
}
{
  const f = await fixture("stale_callback");
  await f.start(f.first);
  const oldTick = [...f.timers.values()][0];
  assert.equal(typeof oldTick, "function");
  await f.start(f.second); f.cancel(f.second);
  oldTick();
  assert.deepEqual(f.counts(), [0, 0]);
  f.tick();
  assert.deepEqual(f.counts(), [1, 1]);
  await f.end();
  assert.equal(f.payloads[0].agent_bridge_cancel_claim_token, f.second);
}
{
  const f = await fixture("direct_escape");
  await f.start(f.first); f.cancel(f.first);
  // Even intent alone does not prove the extension called abort.
  await f.end();
  assert.equal(f.payloads[0].agent_bridge_cancel_claim_token, undefined);
  assert.match(f.payloads[0].agent_bridge_error, /aborted/);
}
for (const kind of ["write_failure", "abort_failure"]) {
  const f = await fixture(kind);
  await f.start(f.first); f.cancel(f.first);
  if (kind === "write_failure") f.failAccepted(); else f.throwAbort();
  f.tick(); f.tick();
  assert.deepEqual(f.counts(), [1, 0]);
  await f.end();
  assert.equal(f.payloads[0].agent_bridge_cancel_claim_token, undefined);
}
for (const event of ["agent_settled", "session_shutdown"]) {
  const f = await fixture(event);
  await f.start(f.first);
  const oldTick = [...f.timers.values()][0];
  assert.equal(typeof oldTick, "function");
  await f.emit(event); await f.emit(event);
  assert.equal(f.timers.size, 0);
  f.cancel(f.first); oldTick();
  assert.deepEqual(f.counts(), [0, 0]);
}
{
  const f = await fixture("normal_completion_wins");
  await f.start(f.first); f.cancel(f.first); f.tick();
  await f.end("stop");
  assert.equal(f.payloads[0].agent_bridge_cancel_claim_token, undefined);
  assert.equal(f.payloads[0].last_assistant_message, "result");
  // Preserve the unmarked-result assertion, then exercise a publishable retry.
  await f.start(f.first); f.tick();
  await f.end("stop", f.records[0].response);
  assert.deepEqual(f.counts(), [1, 1]);
}
{
  const f = await fixture("two_claims_and_retry");
  for (const token of [f.first, f.second]) {
    await f.start(token); f.cancel(token); f.tick();
    await f.end();
    assert.equal(f.payloads.at(-1).agent_bridge_cancel_claim_token, token);
    // An automatic retry with the same claim must not call abort again.
    await f.start(token); f.tick();
    await f.end("error");
    assert.equal(f.payloads.at(-1).agent_bridge_cancel_claim_token, undefined);
  }
  assert.deepEqual(f.counts(), [2, 2]);
}
{
  const f = await fixture("settlement_without_aborted_end");
  await f.start(f.first);
  await f.emit("agent_end", { messages: [{ role: "assistant", stopReason: "error" }] });
  f.cancel(f.first); f.tick();
  await f.emit("agent_settled");
  assert.deepEqual(f.counts(), [0, 0]);
  assert.equal(f.payloads[0].agent_bridge_cancel_claim_token, undefined);
  assert.match(f.payloads[0].agent_bridge_error, /error/);
}

{
  const f = await fixture("retry_without_before_agent_start");
  await f.start(f.first);
  const oldTick = [...f.timers.values()][0];
  await f.emit("agent_end", { messages: [{ role: "assistant", stopReason: "error" }] });
  // Pi can retry the same claim without another before_agent_start event.
  await f.emit("agent_start");
  f.cancel(f.first);
  oldTick();
  assert.deepEqual(f.counts(), [0, 0]);
  f.tick();
  assert.deepEqual(f.counts(), [1, 1]);
  await f.end();
  assert.equal(f.payloads[0].agent_bridge_cancel_claim_token, f.first);
}
{
  const f = await fixture("partial_json_then_valid_record");
  await f.start(f.first);
  fs.writeFileSync(join(f.dir, "cancel.json"), '{"schema":1,"claim_token":');
  f.tick();
  assert.deepEqual(f.counts(), [0, 0]);
  f.cancel(f.first); f.tick(); f.tick();
  assert.deepEqual(f.counts(), [1, 1]);
  await f.end();
  assert.equal(f.payloads[0].agent_bridge_cancel_claim_token, f.first);
}
for (const [name, accepted, errorMessage, expected] of [
  ["tool_abort_accepted", true, "This operation was aborted", true],
  ["tool_abort_direct_escape", false, "This operation was aborted", undefined],
  ["tool_network_error", true, "Network connection failed", undefined],
  ["tool_abort_message_not_exact", true, "This operation was aborted elsewhere", undefined],
  ["tool_abort_wrong_accepted_claim", "wrong", "This operation was aborted", undefined],
]) {
  const f = await fixture(name);
  await f.start(f.first); f.cancel(f.first);
  if (accepted === true) f.tick();
  if (accepted === "wrong") {
    fs.writeFileSync(join(f.dir, "pi-cancel-accepted.json"), JSON.stringify({ schema: 1, claim_token: f.second }));
  }
  await f.emit("agent_end", { messages: [
    { role: "assistant", stopReason: "toolUse", content: [{ type: "toolCall" }] },
    { role: "toolResult", isError: true },
    { role: "assistant", stopReason: "error", errorMessage, content: [] },
  ] });
  assert.equal(f.timers.size, 0);
  await f.emit("agent_settled");
  assert.equal(f.payloads[0].agent_bridge_error, `Pi turn ended with error: ${errorMessage}`);
  assert.equal(f.payloads[0].agent_bridge_cancel_claim_token, expected === true ? f.first : expected, name);
}

{
  const f = await fixture("normal_complete");
  await f.start(f.first);
  await f.end("stop", f.records[0].response);
}
fs.writeFileSync(join(directory, "payloads.json"), JSON.stringify(fixtures));
}
