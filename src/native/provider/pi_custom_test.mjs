import assert from "node:assert/strict";
import * as fs from "node:fs";
import { join } from "node:path";
import vm from "node:vm";

const directory = process.argv[2];
const records = JSON.parse(fs.readFileSync(join(directory, "inputs.json"), "utf8"));
const source = fs.readFileSync(join(directory, "extension.js"), "utf8")
  .replace(/^import .*;\n/gm, "")
  .replace("export default function (pi)", "function install(pi)");

for (const kind of ["custom", "missing", "unknown", "invalid", "legacy", "user"]) {
  const handlers = new Map();
  const payloads = [];
  const sandbox = vm.createContext({
    process: { env: { AGENT_BRIDGE_NATIVE_SESSION_DIR: directory, AGENT_BRIDGE_EXECUTABLE: "fake" }, pid: 1 },
    join, readFileSync: fs.readFileSync, writeFileSync: fs.writeFileSync,
    renameSync: fs.renameSync, unlinkSync: fs.unlinkSync,
    spawnSync(_command, _args, options) { payloads.push(JSON.parse(options.input)); return { status: 0 }; },
    setInterval() { return 1; }, clearInterval() {},
  });
  vm.runInContext(source, sandbox);
  sandbox.install({ on(name, handler) { handlers.set(name, handler); } });
  const ctx = { sessionManager: { getSessionId: () => "pi-custom", getLeafId: () => "leaf" }, ui: { notify() {} } };
  const emit = (name, event = {}) => handlers.get(name)?.(event, ctx);
  fs.writeFileSync(join(directory, "turn.claim"), records[0].claim);
  await emit("before_agent_start", { prompt: records[0].prompt });
  await emit("agent_start");
  fs.writeFileSync(join(directory, "turn.claim"), records[1].claim);
  const event = kind === "custom" ? { messageType: "custom", message: {} }
    : kind === "missing" ? {}
    : kind === "legacy" ? { prompt: records[1].prompt }
    : { messageType: kind === "invalid" ? null : kind, prompt: records[1].prompt };
  try { await emit("before_agent_start", event); } catch {}
  await emit("agent_start");
  await emit("agent_end", { messages: [{ role: "assistant", stopReason: "stop", content: [{ type: "text", text: "custom result" }],
    usage: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2 } }] });
  await emit("agent_settled");
  assert.equal(payloads.length, ["legacy", "user"].includes(kind) ? 1 : 0,
    `${kind} event must reset correlation rather than retaining the prior run`);
}
