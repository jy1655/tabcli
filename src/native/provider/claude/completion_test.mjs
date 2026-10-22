import assert from "node:assert/strict";
import fs from "node:fs";
import { spawnSync } from "node:child_process";
import { pathToFileURL } from "node:url";

const [modulePath, directory, executable, scenario] = process.argv.slice(2);
const pending = JSON.parse(fs.readFileSync(`${directory}/claude-pending-turn.json`, "utf8"));
const conversation = "00000000-0000-4000-8000-000000000249";
const turnId = "00000000-0000-4000-8000-000000000250";
const max = 4 * 1024 * 1024;
const handlers = new Map();
const calls = [];
const logs = [];
let completeAttempts = 0;
let continuationAttempts = 0;
const $ = {
  env: { get: async name => name === "AGENT_BRIDGE_EXECUTABLE" ? executable : undefined },
  session: { id: async () => conversation },
  ui: { log: text => logs.push(text) },
  process: { run: async (argv, init) => {
    assert.deepEqual(argv, [executable, "native-hook", "claude"]);
    assert.equal(init.timeoutMs, 30000);
    const wire = JSON.parse(init.stdin);
    calls.push({ event: wire.event, bytes: Buffer.byteLength(init.stdin), stdin: init.stdin });
    if (wire.event === "turn.complete") {
      completeAttempts++;
      if (scenario === "helper_failure") return { exitCode: 1 };
      if (scenario === "retry_before" && completeAttempts === 1) throw new Error("transport rejected");
    }
    if (wire.event === "turn.complete" && wire.data.turnId !== turnId) continuationAttempts++;
    const result = spawnSync(executable, ["native-hook", "claude"], {
      input: init.stdin, encoding: "utf8", env: { ...process.env,
        AGENT_BRIDGE_NATIVE_SESSION_DIR: directory },
    });
    assert.equal(result.status, 0, result.stderr);
    if (scenario === "retry_after" && wire.event === "turn.complete" && completeAttempts === 1) return { exitCode: 1 };
    if (scenario === "continuation_retry_after" && wire.event === "turn.complete"
        && wire.data.turnId !== turnId && continuationAttempts === 1) return { exitCode: 1 };
    return { exitCode: result.status, stdout: result.stdout, stderr: result.stderr };
  } },
};
const { register } = await import(pathToFileURL(modulePath).href);
register((name, fn) => { assert(!handlers.has(name)); handlers.set(name, fn); });
assert.deepEqual([...handlers.keys()], ["session.start", "turn.start", "turn.complete"]);
let nextCalls = 0;
async function fire(name, e) {
  const sentinel = { name };
  const before = nextCalls;
  const result = await handlers.get(name)($, e, async received => {
    assert.equal(received, e); nextCalls++; return sentinel;
  });
  assert.equal(result, sentinel);
  assert.equal(nextCalls, before + 1);
}
await fire("session.start", {});
const peerBody = pending.expected_message.replace(/<(\/?cross-session-message)>/gi, "<\\$1>");
const start = { turnId, text: `<cross-session-message from="uds:\\\\.\\pipe\\LOCAL\\sender" from-name="sender" from-mode="prompting">\n${peerBody}\n</cross-session-message>` };
await fire("turn.start", start);
let answer = "한😀\n".repeat(15000);
assert(answer.length > 32767);
const suffix = `\n${pending.marker}`;
const continuing = scenario.startsWith("continuation");
const data = { turnId, reason: "answer", isAborted: false, answer: "" };
if (scenario === "boundary" || scenario === "oversize" || scenario === "continuation_oversize") {
  const overhead = Buffer.byteLength(JSON.stringify({ agent_bridge_claude_mod: 1, event: "turn.complete", conversation, data }));
  answer = "x".repeat(max - overhead - (Buffer.byteLength(JSON.stringify(suffix)) - 2) + (scenario.endsWith("oversize") ? 1 : 0));
}
if (continuing || scenario === "unmarked") {
  const before = new Map(fs.readdirSync(directory).filter(name => name.endsWith(".json") || name === "turn.claim")
    .map(name => [name, fs.readFileSync(`${directory}/${name}`)]));
  await fire("turn.complete", { ...data, answer: "background interim" });
  assert.equal(fs.readdirSync(`${directory}/events`).length, 0);
  assert(fs.existsSync(`${directory}/turn.claim`));
  const observed = new Map(fs.readdirSync(directory).filter(name => name.endsWith(".json") || name === "turn.claim")
    .map(name => [name, fs.readFileSync(`${directory}/${name}`)]));
  for (const [name, bytes] of before) assert.deepEqual(fs.readFileSync(`${directory}/${name}`), bytes);
  const namesBeforeInspect = fs.readdirSync(directory).sort();
  const inspection = spawnSync(executable, ["inspect", directory.split(/[\\/]/).at(-1), "--json"],
    { encoding: "utf8", env: process.env });
  assert.equal(inspection.status, 0, inspection.stderr);
  const evidence = JSON.parse(inspection.stdout).provider_completion;
  assert.equal(evidence.state, "unmarked_bound_turn");
  assert.equal(evidence.bound_turn_id, turnId);
  assert.equal(evidence.selected_completion_turn_id, null);
  for (const [name, bytes] of observed) assert.deepEqual(fs.readFileSync(`${directory}/${name}`), bytes);
  assert.deepEqual(fs.readdirSync(directory).sort(), namesBeforeInspect);
  if (continuing) {
    data.turnId = "00000000-0000-4000-8000-000000000251";
    await fire("turn.start", { turnId: data.turnId, text: "<task-notification>finished</task-notification>" });
  }
}
data.answer = answer + suffix;
if (scenario === "subagent") data.agentId = "child";
if (scenario !== "unmarked") await fire("turn.complete", data);
const reports = calls.filter(call => call.event === "turn.complete" || call.event === "oversize");
if (scenario === "subagent") assert.equal(reports.length, 0);
else if (scenario === "continuation_retry_after") {
  assert.equal(reports.length, 3);
  assert.equal(reports[1].stdin, reports[2].stdin);
} else if (continuing) assert.equal(reports.length, 2);
else if (scenario === "retry_before" || scenario === "retry_after" || scenario === "helper_failure") {
  assert.equal(reports.length, 2);
  assert.equal(reports[0].stdin, reports[1].stdin);
} else assert.equal(reports.length, 1);
if (scenario.endsWith("oversize")) {
  assert.equal(reports.at(-1).event, "oversize");
  assert(reports.at(-1).bytes < 512);
} else if (scenario === "boundary") assert.equal(reports[0].bytes, max);
if (scenario === "helper_failure") assert.equal(logs.length, 1);
const before = calls.length;
for (const agentId of ["child", "", null]) {
  await fire("turn.start", { ...start, agentId });
  await fire("turn.complete", { ...data, agentId });
}
assert.equal(calls.length, before);
fs.writeFileSync(`${directory}/harness-result.json`, JSON.stringify({
  expected_answer: answer, calls: calls.map(({ event, bytes }) => ({ event, bytes })), nextCalls,
}));
