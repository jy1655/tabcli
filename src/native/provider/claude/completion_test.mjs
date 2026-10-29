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
let questionAttempts = 0;
const $ = {
  env: { get: async name => name === "AGENT_BRIDGE_EXECUTABLE" ? executable : undefined },
  session: { id: async () => conversation },
  ui: { log: text => logs.push(text) },
  process: { run: async (argv, init) => {
    assert.deepEqual(argv, [executable, "native-hook", "claude"]);
    const wire = JSON.parse(init.stdin);
    assert.equal(init.timeoutMs, wire.event === "question.emissions" ? 5000 : 30000);
    calls.push({ event: wire.event, bytes: Buffer.byteLength(init.stdin), stdin: init.stdin });
    if (wire.event === "turn.complete") {
      completeAttempts++;
      if (scenario === "helper_failure") return { exitCode: 1 };
      if (scenario === "retry_before" && completeAttempts === 1) throw new Error("transport rejected");
    }
    if (wire.event === "turn.complete" && wire.data.turnId !== turnId) continuationAttempts++;
    if (wire.event === "question.emissions") questionAttempts++;
    const result = spawnSync(executable, ["native-hook", "claude"], {
      input: init.stdin, encoding: "utf8", env: { ...process.env,
        AGENT_BRIDGE_NATIVE_SESSION_DIR: directory },
    });
    assert.equal(result.status, 0, result.stderr);
    if (scenario === "retry_after" && wire.event === "turn.complete" && completeAttempts === 1) return { exitCode: 1 };
    if (scenario === "continuation_retry_after" && wire.event === "turn.complete"
        && wire.data.turnId !== turnId && continuationAttempts === 1) return { exitCode: 1 };
    if (scenario === "questions_retry" && wire.event === "question.emissions" && questionAttempts === 1) return { exitCode: 1 };
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
let reportOffset = 0;
if (scenario === "coexistence") {
  // Synthetic target events: these do not emulate session.receive or establish hook order.
  const claim = fs.readFileSync(`${directory}/turn.claim`, "utf8").trim();
  const protectedNames = ["turn.claim", "claude-pending-turn.json", "status.json",
    `requests/${claim}.json`];
  const protectedBytes = protectedNames.map(name => fs.readFileSync(`${directory}/${name}`));
  const receipt = JSON.parse(protectedBytes.at(-1));
  const session = directory.split(/[\\/]/).at(-1);
  function snapshot(root = directory) {
    return fs.readdirSync(root, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))
      .flatMap(entry => {
        const path = `${root}/${entry.name}`;
        return entry.isDirectory() ? [[path, null], ...snapshot(path)] : [[path, fs.readFileSync(path)]];
      });
  }
  function retained(label) {
    for (const [index, name] of protectedNames.entries()) {
      assert.deepEqual(fs.readFileSync(`${directory}/${name}`), protectedBytes[index], `${label}: ${name}`);
    }
    assert.deepEqual(fs.readdirSync(`${directory}/events`), [], label);
    for (const kind of ["bound", "candidate", "continuation"]) {
      assert.equal(fs.existsSync(`${directory}/claude-mod-${kind}-${claim}.json`), false, label);
    }
    const before = snapshot();
    const query = spawnSync(executable, ["result", session, "--request", receipt.request_id, "--json"],
      { encoding: "utf8", env: process.env });
    assert.equal(query.status, 0, query.stderr);
    const result = JSON.parse(query.stdout);
    assert.equal(result.request_state, "pending", label);
    assert.equal(result.result, null, label);
    assert.deepEqual(snapshot(), before, `${label}: result must be read-only`);
  }
  retained("no target turn (a possible consumed-input outcome)");
  await fire("turn.complete", { turnId, reason: "answer", isAborted: false,
    answer: `marked answer without a target start\n${pending.marker}` });
  retained("marked answer alone supplies no input binding");
  const variants = [
    ["rewritten body with marker intact", start.text.replace("Source: external", "Source: changed")],
    ["stripped peer envelope", peerBody],
    ["combined queued input", `${start.text}\nAnother queued input`],
    ["unrelated turn", "A separate manual request"],
  ];
  for (const [index, [label, text]] of variants.entries()) {
    assert.notEqual(text, start.text, label);
    const rejectedTurn = `00000000-0000-4000-8000-${String(260 + index).padStart(12, "0")}`;
    await fire("turn.start", { turnId: rejectedTurn, text });
    retained(`${label}: start`);
    await fire("turn.complete", { turnId: rejectedTurn, reason: "answer", isAborted: false,
      answer: `nonmatching turn copied the pending marker\n${pending.marker}` });
    retained(`${label}: marked final`);
  }
  reportOffset = calls.length;
}
await fire("turn.start", start);
if (scenario.startsWith("questions")) {
  const questionHandlers = new Map();
  const { register: registerQuestions } = await import(new URL("./questions.mjs", pathToFileURL(modulePath)).href);
  registerQuestions((name, fn) => questionHandlers.set(name, fn));
  assert.deepEqual([...questionHandlers.keys()], ["turn.complete", "turn.step"]);
  const question = { question: "Which baseline? 한😀", header: "Baseline", multiSelect: true,
    options: [{ label: "Current", description: "Current baseline" }, { label: "Previous", description: "Previous baseline" }] };
  async function step({ index = 0, agentId, origin = { plugin: "engine", tier: "core" },
      broken = false, mismatch = false, cancel = false, clear = false, large = false,
      empty = false, noTrace = false, rewritten = false, stopReason = "tool_use", sessionFailure = false } = {}) {
    const e = { turnId, index, model: "fixture", messageCount: 1 };
    if (agentId !== undefined) e.agentId = agentId;
    const chunks = [{ kind: "text", index: 0, text: "Question follows", ref: 1 },
      { kind: "tool", index: 1, id: "toolu-question", name: "AskUserQuestion", ref: 2 },
      { kind: "engine", ref: 3 }, { kind: "input", index: 1, json: "{}", ref: 4 }];
    const core = { turnId: mismatch ? "foreign" : turnId, index, answer: "Question follows", stopReason,
      toolUses: [{ name: "Read", input: {} }, { name: "AskUserQuestion", input: { questions: [
        large ? { ...question, question: "x".repeat(max + 1) } : question,
      ] } }], usage: null };
    if (empty) core.toolUses = [];
    const response = rewritten ? { ...core, toolUses: [] } : core;
    const error = new Error("model step aborted");
    const next = async function* (received) {
      assert.equal(received, e);
      for (const chunk of chunks) yield chunk;
      if (broken) throw error;
      if (clear) $.session.id = async () => "00000000-0000-4000-8000-000000000999";
      next.trace = noTrace ? [] : [{ plugin: "engine", tier: "core", returned: core }];
      return response;
    };
    next.origin = origin;
    if (sessionFailure) $.session.id = async () => { throw new Error("unavailable"); };
    const stream = questionHandlers.get("turn.step")($, e, next);
    assert.equal((await stream.next()).value, chunks[0]);
    if (cancel) { await stream.return(); return; }
    assert.equal((await stream.next()).value, chunks[1]);
    assert.equal((await stream.next()).value, chunks[2]);
    assert.equal((await stream.next()).value, chunks[3]);
    if (broken) { await assert.rejects(stream.next(), actual => actual === error); return; }
    const final = await stream.next();
    assert.equal(final.done, true);
    assert.equal(final.value, response);
    $.session.id = async () => conversation;
  }
  const before = calls.length;
  for (const agentId of ["child", "", null]) await step({ agentId });
  await step({ origin: { plugin: "another-plugin", tier: "user" } });
  await step({ index: 10, broken: true });
  await step({ index: 11, mismatch: true });
  await step({ index: 12, cancel: true });
  await step({ index: 13, empty: true });
  await step({ index: 14, noTrace: true });
  await step({ index: 15, sessionFailure: true });
  assert.equal(calls.length, before);
  if (scenario === "questions_oversize") {
    await step({ large: true });
    const wire = JSON.parse(calls.at(-1).stdin);
    assert.deepEqual(wire.data.items, []);
    assert.equal(wire.data.incomplete, true);
    assert(calls.at(-1).bytes < 512);
  }
  await step({ index: 1, clear: true });
  const claim = fs.readFileSync(`${directory}/turn.claim`, "utf8").trim();
  const recordPath = `${directory}/claude-questions-${claim}.json`;
  let record = fs.readFileSync(recordPath);
  if (scenario !== "questions_retry") {
    await step({ index: 1, empty: true });
    assert.equal(JSON.parse(fs.readFileSync(recordPath)).incomplete, true);
    assert.deepEqual(JSON.parse(fs.readFileSync(recordPath)).items, JSON.parse(record).items);
    record = fs.readFileSync(recordPath);
    await step({ index: 1 });
    assert.deepEqual(fs.readFileSync(recordPath), record);
  }
  assert.equal(fs.readdirSync(`${directory}/events`).length, 0);
  assert.equal(fs.readFileSync(`${directory}/turn.claim`, "utf8").trim(), claim);
  const receipt = JSON.parse(fs.readFileSync(`${directory}/requests/${claim}.json`, "utf8"));
  const address = `${directory.split(/[\\/]/).at(-1)}/${receipt.request_id}`;
  function wait(args) {
    const result = spawnSync(executable, ["wait", address, "--timeout-secs", "1", "--json", ...args],
      { encoding: "utf8", env: process.env });
    return JSON.parse(result.stdout);
  }
  const observation = wait(["--questions"]);
  assert.equal(observation.timed_out, false);
  assert.equal(observation.ended, null);
  assert.equal(observation.question.question_emissions[0].questions[0].title, question.question);
  assert.equal(observation.question.question_emissions[0].id, "step:1/tool:1");
  assert.equal(wait(["--questions", "--after-question", observation.question.after_question]).timed_out, true);
  assert.equal(wait([]).timed_out, true);
  assert.deepEqual(fs.readFileSync(recordPath), record);
  if (scenario === "questions") {
    for (const [index, stopReason] of [[2, null], [3, "refusal"]]) {
      const before = calls.length;
      await step({ index, stopReason });
      assert.equal(calls.length, before + 1);
      const wire = JSON.parse(calls.at(-1).stdin);
      assert.equal(wire.event, "question.emissions");
      assert.equal(wire.data.incomplete, true);
      assert.deepEqual(wire.data.items, []);
    }
    const before = calls.length;
    await step({ index: 4, rewritten: true });
    assert.equal(calls.length, before + 1);
    const wire = JSON.parse(calls.at(-1).stdin);
    assert.equal(wire.event, "question.emissions");
    assert.equal(wire.data.incomplete, true);
    assert.deepEqual(wire.data.items, [{ index: 1, questions: [question] }]);
    const after = JSON.parse(fs.readFileSync(recordPath));
    assert.equal(after.incomplete, true);
    assert.equal(after.items.length, 2);
    assert.equal(after.items[1].id, "step:4/tool:1");
    assert.equal(after.items.some(item => /step:[23]\//.test(item.id)), false);
  }
}
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
const reports = calls.slice(reportOffset).filter(call => call.event === "turn.complete" || call.event === "oversize");
if (scenario === "subagent") assert.equal(reports.length, 0);
else if (scenario === "continuation_retry_after") {
  assert.equal(reports.length, 3);
  assert.equal(reports[1].stdin, reports[2].stdin);
} else if (continuing) assert.equal(reports.length, 2);
else if (scenario === "retry_before" || scenario === "retry_after" || scenario === "helper_failure") {
  assert.equal(reports.length, 2);
  assert.equal(reports[0].stdin, reports[1].stdin);
} else assert.equal(reports.length, 1);
if (scenario === "oversize" || scenario === "continuation_oversize") {
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
