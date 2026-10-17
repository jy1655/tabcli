// Executes the installed extension against sanitized Pi 1.2.0 assistant messages and
// checks the usage it reports beside each result.
import assert from "node:assert/strict";
import * as fs from "node:fs";
import { join } from "node:path";
import vm from "node:vm";

const directory = process.argv[2];
const source = fs.readFileSync(join(directory, "extension.js"), "utf8")
  .replace(/^import .*;\n/gm, "")
  .replace("export default function (pi)", "function install(pi)");
const inputs = JSON.parse(fs.readFileSync(join(directory, "inputs.json"), "utf8"));
const shapes = JSON.parse(fs.readFileSync(join(directory, "fixtures.json"), "utf8"));
const fixtures = [];

// A sanitized live message, optionally with another text body or another usage object.
function message(kind, overrides = {}) {
  const base = structuredClone(shapes[kind]);
  const { text, usage, ...rest } = overrides;
  if (text !== undefined) base.content = [{ type: "text", text }];
  if (usage !== undefined) {
    if (usage === null) delete base.usage; else base.usage = usage;
  }
  return { ...base, ...rest };
}

function fixture(name) {
  const dir = join(directory, name);
  const records = inputs[name];
  const record = (token) => records.find((r) => r.token === token);
  const handlers = new Map();
  const timers = new Map();
  const payloads = [];
  let nextTimer = 0;
  let failDelivery = false;
  const context = vm.createContext({
    process: { env: { AGENT_BRIDGE_NATIVE_SESSION_DIR: dir, AGENT_BRIDGE_EXECUTABLE: "fake" }, pid: 1, platform: "test" },
    join, readFileSync: fs.readFileSync, unlinkSync: fs.unlinkSync,
    writeFileSync: fs.writeFileSync, renameSync: fs.renameSync,
    spawnSync(_executable, _args, options) {
      payloads.push(JSON.parse(options.input));
      return failDelivery ? { status: 1, stderr: "refused" } : { status: 0 };
    },
    setInterval(callback, delay) { timers.set(++nextTimer, callback); return nextTimer; },
    clearInterval(id) { timers.delete(id); },
    setTimeout(callback) { callback(); },
  });
  vm.runInContext(source, context);
  context.install({ on(name, callback) { handlers.set(name, callback); } });
  const ctx = {
    abort() {},
    sessionManager: { getSessionId: () => `pi-${name}`, getLeafId: () => `leaf-${name}` },
    ui: { notify() {} },
  };
  const emit = async (name, event = {}) => handlers.get(name)?.(event, ctx);
  const start = async (token) => {
    fs.writeFileSync(join(dir, "turn.claim"), record(token).claim);
    await emit("before_agent_start", { prompt: record(token).prompt });
    await emit("agent_start");
  };
  const response = (token) => record(token).response;
  const tick = () => { for (const callback of [...timers.values()]) callback(); };
  fixtures.push({ name, payloads });
  return {
    dir, first: records[0].token, second: records[1]?.token, record, emit, start, tick, response, payloads,
    failDelivery: () => { failDelivery = true; },
  };
}

const usageOf = (payload) => payload.usage;
const liveSums = {
  input_tokens: 170 + 623, output_tokens: 53 + 5, cache_read_tokens: 8320 + 8320,
  cache_write_tokens: 0, reasoning_tokens: 23 + 0, total_tokens: 8543 + 8948,
};

{
  // The live second request: one run, a tool call and the final answer.
  const f = fixture("one_run_two_messages");
  await f.start(f.first);
  await f.emit("agent_end", { messages: [
    message("tool_use"), message("tool_result"), message("done", { text: f.response(f.first) }),
  ] });
  await f.emit("agent_settled");
  assert.equal(f.payloads.length, 1);
  assert.equal(f.payloads[0].usage_error, undefined);
  assert.deepEqual(usageOf(f.payloads[0]), {
    coverage: "complete", runs: 1, assistant_messages: 2, ...liveSums,
  });
}
{
  const f = fixture("two_runs");
  await f.start(f.first);
  await f.emit("agent_end", { messages: [message("ready")] });
  await f.emit("agent_start");
  await f.emit("agent_end", { messages: [message("ready", { text: f.response(f.first) })] });
  await f.emit("agent_settled");
  assert.equal(f.payloads.length, 1);
  assert.deepEqual(usageOf(f.payloads[0]), {
    coverage: "complete", runs: 2, assistant_messages: 2, input_tokens: 16800, output_tokens: 10,
    cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0, total_tokens: 16810,
  });
}
{
  // Pi retries the same claim without another before_agent_start; the failed run counts.
  const f = fixture("retry_without_before_agent_start");
  await f.start(f.first);
  await f.emit("agent_end", { messages: [
    message("ready", { stopReason: "error", errorMessage: "overloaded",
      usage: { input: 1, output: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0, totalTokens: 1 } }),
  ] });
  await f.emit("agent_start");
  await f.emit("agent_end", { messages: [message("ready", { text: f.response(f.first) })] });
  await f.emit("agent_settled");
  assert.deepEqual(usageOf(f.payloads[0]), {
    coverage: "uncertain", runs: 2, assistant_messages: 2, input_tokens: 8401, output_tokens: 5,
    cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0, total_tokens: 8406,
  });
  assert.equal(f.payloads[0].last_assistant_message, f.response(f.first));
}
{
  const f = fixture("reasoning_omitted_unless_everywhere");
  await f.start(f.first);
  const noReasoning = structuredClone(shapes.tool_use.usage);
  delete noReasoning.reasoning;
  await f.emit("agent_end", { messages: [
    message("tool_use", { usage: noReasoning }), message("tool_result"),
    message("done", { text: f.response(f.first) }),
  ] });
  await f.emit("agent_settled");
  const usage = usageOf(f.payloads[0]);
  assert.equal(usage.reasoning_tokens, undefined);
  assert.equal(usage.total_tokens, liveSums.total_tokens);
  assert.equal(usage.coverage, "complete");
}
for (const [name, usage, field] of [
  ["missing_usage", null, "assistant message has no usage"],
  ["fractional_operand", { input: 1.5, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 1 }, "usage.input"],
  ["negative_operand", { input: 1, output: -1, cacheRead: 0, cacheWrite: 0, totalTokens: 1 }, "usage.output"],
  ["unsafe_operand", { input: 1, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 2 ** 53 }, "usage.totalTokens"],
  ["unsafe_sum", { input: 1, output: 0, cacheRead: Number.MAX_SAFE_INTEGER, cacheWrite: 0, totalTokens: 1 }, "usage.cacheRead sum"],
  ["fractional_reasoning", { input: 1, output: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0.5, totalTokens: 1 }, "usage.reasoning"],
]) {
  const f = fixture(name);
  await f.start(f.first);
  const first = name === "unsafe_sum" ? message("tool_use", { usage: { ...usage } }) : message("tool_use");
  await f.emit("agent_end", { messages: [
    first, message("tool_result"), message("done", { text: f.response(f.first), usage }),
  ] });
  await f.emit("agent_settled");
  assert.equal(f.payloads.length, 1, name);
  assert.equal(f.payloads[0].usage, undefined, name);
  assert.match(f.payloads[0].usage_error, new RegExp(field.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")), name);
  assert.ok(f.payloads[0].usage_error.length <= 200, name);
  assert.equal(f.payloads[0].last_assistant_message, f.response(f.first), name);
}
{
  const f = fixture("error_run_is_uncertain");
  await f.start(f.first);
  await f.emit("agent_end", { messages: [
    message("tool_use"), message("tool_result"),
    message("done", { stopReason: "error", errorMessage: "Network connection failed", content: [] }),
  ] });
  await f.emit("agent_settled");
  assert.match(f.payloads[0].agent_bridge_error, /Network connection failed/);
  assert.deepEqual(usageOf(f.payloads[0]), { coverage: "uncertain", runs: 1, assistant_messages: 2, ...liveSums });
}
{
  const f = fixture("cancelled_keeps_usage");
  await f.start(f.first);
  fs.writeFileSync(join(f.dir, "cancel.json"), f.record(f.first).cancel);
  f.tick();
  await f.emit("agent_end", { messages: [
    message("tool_use"), message("tool_result"),
    message("done", { stopReason: "aborted", content: [] }),
  ] });
  await f.emit("agent_settled");
  assert.equal(f.payloads[0].agent_bridge_cancel_claim_token, f.first);
  assert.deepEqual(usageOf(f.payloads[0]), { coverage: "uncertain", runs: 1, assistant_messages: 2, ...liveSums });
}
{
  // A replaced claim starts its own accumulation; the first claim's run is not carried.
  const f = fixture("claim_replaced_resets");
  await f.start(f.first);
  await f.emit("agent_end", { messages: [message("ready")] });
  await f.start(f.second);
  await f.emit("agent_end", { messages: [message("done", { text: f.response(f.second) })] });
  await f.emit("agent_settled");
  assert.equal(f.payloads.length, 1);
  assert.equal(f.payloads[0].agent_bridge_claim_token, f.second);
  assert.deepEqual(usageOf(f.payloads[0]), {
    coverage: "complete", runs: 1, assistant_messages: 1, input_tokens: 623, output_tokens: 5,
    cache_read_tokens: 8320, cache_write_tokens: 0, reasoning_tokens: 0, total_tokens: 8948,
  });
}
{
  const f = fixture("delivery_failure_persists_usage");
  await f.start(f.first);
  f.failDelivery();
  await f.emit("agent_end", { messages: [message("ready", { text: f.response(f.first) })] });
  await f.emit("agent_settled");
  const signal = JSON.parse(fs.readFileSync(join(f.dir, "pi-hook-failure.json"), "utf8"));
  assert.equal(signal.claim_token, f.first);
  assert.match(signal.error, /Pi result delivery failed/);
  assert.deepEqual(signal.usage, usageOf(f.payloads[0]));
  assert.equal(signal.usage_error, undefined);
}
{
  const f = fixture("delivery_failure_persists_usage_error");
  await f.start(f.first);
  f.failDelivery();
  await f.emit("agent_end", { messages: [message("ready", { text: f.response(f.first), usage: null })] });
  await f.emit("agent_settled");
  const signal = JSON.parse(fs.readFileSync(join(f.dir, "pi-hook-failure.json"), "utf8"));
  assert.equal(signal.usage, undefined);
  assert.equal(signal.usage_error, "assistant message has no usage");
}
{
  const f = fixture("session_shutdown_resets");
  await f.start(f.first);
  await f.emit("agent_end", { messages: [message("ready")] });
  await f.emit("session_shutdown");
  await f.start(f.first);
  await f.emit("agent_end", { messages: [message("ready", { text: f.response(f.first) })] });
  await f.emit("agent_settled");
  assert.equal(usageOf(f.payloads[0]).runs, 1);
}
fs.writeFileSync(join(directory, "payloads.json"), JSON.stringify(fixtures));
