import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import * as fs from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import vm from "node:vm";

const root = process.argv[2];
const inputs = JSON.parse(fs.readFileSync(join(root, "inputs.json"), "utf8"));
const source = fs.readFileSync(join(root, "extension.mjs"), "utf8")
  .replace(/^import .*;\n/gm, "")
  .replace("export default function (pi)", "function install(pi)");
const results = [];

for (const [name, records] of Object.entries(inputs)) {
  const directory = join(root, name);
  const first = records[0];
  const second = records[1];
  fs.writeFileSync(join(directory, "turn.claim"), first.claim);
  fs.writeFileSync(join(directory, "pi-pending-turn.json"), first.pending);
  const handlers = new Map();
  const timers = new Map();
  const payloads = [];
  let publications = 0;
  let attempts = 0;
  let aborts = 0;
  let failReceipt = false;
  let failResult = false;
  let retireOnRead = false;
  let sessionId = "pi-input";
  const ctx = {
    abort() { aborts += 1; },
    sessionManager: { getSessionId: () => sessionId, getLeafId: () => "leaf" },
    ui: { notify() {} },
  };
  if (name === "shipping") {
    process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR = directory;
    const { default: install } = await import(pathToFileURL(join(root, "extension.mjs")));
    install({ on(event, handler) { handlers.set(event, handler); } });
  } else {
    const sandbox = vm.createContext({
      process: { platform: name === "macos" ? "darwin" : "win32", pid: 1,
        env: { AGENT_BRIDGE_NATIVE_SESSION_DIR: directory, AGENT_BRIDGE_EXECUTABLE: "fake" } },
      Buffer, TextDecoder, createHash, join,
      closeSync: fs.closeSync, fstatSync: fs.fstatSync, lstatSync: fs.lstatSync,
      openSync: fs.openSync, readSync: fs.readSync, unlinkSync: fs.unlinkSync,
      readFileSync(path, ...args) {
        const bytes = fs.readFileSync(path, ...args);
        if (retireOnRead && path === join(directory, "turn.claim")) {
          fs.unlinkSync(path);
          retireOnRead = false;
        }
        return bytes;
      },
      writeFileSync(path, ...args) {
        if (path.includes(".pi-input-")) attempts += 1;
        if (failReceipt && name === "write_failure" && path.includes(".pi-input-")) throw new Error("temporary write failed");
        if (failResult && path.includes(".pi-hook-failure-")) throw new Error("result publication failed");
        fs.writeFileSync(path, ...args);
      },
      renameSync(from, to) {
        if (to.includes("pi-input-accepted-")) {
          publications += 1;
          if (failReceipt && name !== "post_rename_failure") throw new Error("receipt publication failed");
        }
        fs.renameSync(from, to);
        if (failReceipt && name === "post_rename_failure" && to.includes("pi-input-accepted-")) throw new Error("post-publication failure");
      },
      spawnSync(_command, _args, options) {
        payloads.push(JSON.parse(options.input));
        return { status: failResult ? 1 : 0 };
      },
      setInterval(callback) { const id = Symbol(); timers.set(id, callback); return id; },
      clearInterval(id) { timers.delete(id); },
      setTimeout(callback) { callback(); },
    });
    vm.runInContext(source, sandbox);
    sandbox.install({ on(event, handler) { handlers.set(event, handler); } });
  }
  const emit = async (event, payload = {}) => handlers.get(event)?.(payload, ctx);
  const receipt = (record) => join(directory, `pi-input-accepted-${record.token}.json`);
  const before = (prompt = first.prompt, extra = {}) => emit("before_agent_start", { messageType: "user", prompt, ...extra });
  await emit("session_start", { reason: "startup" });
  const startup = JSON.parse(fs.readFileSync(join(directory, "pi-startup-ready.json"), "utf8"));
  assert.equal(startup.input_ack_schema, name === "macos" || (name === "shipping" && process.platform !== "win32") ? undefined : 1);

  if (name === "undelivered") {
    await before(); await emit("agent_start");
    failResult = true;
    await emit("agent_end", { messages: [{ role: "assistant", stopReason: "stop", content: [{ type: "text", text: "old result" }] }] });
    await emit("agent_settled");
    fs.writeFileSync(join(directory, "turn.claim"), second.claim);
    fs.writeFileSync(join(directory, "pi-pending-turn.json"), second.pending);
    await before(second.prompt); await emit("agent_start");
    assert.ok(fs.existsSync(receipt(second)), "prior undelivered result cannot suppress the new input receipt");
    assert.equal(publications, 2);
  } else {
    if (name === "manual") await before("ordinary manual text");
    else if (name === "marker") await before(first.marker);
    else if (name === "altered") await before(first.prompt.replace("synthetic", "changed"));
    else if (name === "other_claim") await before(second.prompt);
    else if (name === "images") await before(first.prompt, { images: [{ type: "image" }] });
    else if (name === "invalid_images") await before(first.prompt, { images: {} });
    else if (["custom", "missing", "unknown"].includes(name)) {
      await before();
      await emit("before_agent_start", name === "custom" ? { messageType: "custom", message: {} }
        : name === "unknown" ? { messageType: "other", prompt: first.prompt } : {});
    } else if (name === "legacy") await emit("before_agent_start", { prompt: first.prompt });
    else if (name === "bad_binding") {
      const pending = JSON.parse(first.pending);
      pending.windows_input.utf8_bytes += 1;
      fs.writeFileSync(join(directory, "pi-pending-turn.json"), JSON.stringify(pending));
      await before();
    } else if (name === "large_pending") {
      fs.writeFileSync(join(directory, "pi-pending-turn.json"), " ".repeat(4097));
      await before();
    } else await before();
    if (name === "replaced") fs.writeFileSync(join(directory, "turn.claim"), second.claim);
    if (name === "retired") fs.unlinkSync(join(directory, "turn.claim"));
    if (name === "retired_after_read") retireOnRead = true;
    if (["publication_failure", "write_failure", "post_rename_failure"].includes(name)) failReceipt = true;
    if (name === "conversation") sessionId = "pi-new-conversation";
    await emit("agent_start");
    const expected = ["exact", "legacy", "repeat", "retry", "retired_after_read", "conversation", "post_rename_failure"].includes(name)
      || (name === "shipping" && process.platform === "win32");
    assert.equal(fs.existsSync(receipt(first)), expected, name);
    if (name === "repeat") { await before(); await emit("agent_start"); }
    if (["retry", "publication_failure", "write_failure", "post_rename_failure"].includes(name)) {
      await emit("agent_start");
      await before(); await emit("agent_start");
    }
    if (["publication_failure", "write_failure", "post_rename_failure"].includes(name)) {
      assert.equal(attempts, 1, "failed publication consumes this claim's only attempt");
      assert.equal(publications, name === "write_failure" ? 0 : 1);
      assert.equal(timers.size, 1, "input I/O failure cannot skip cancellation observation");
    }
    if (name !== "shipping") assert.equal(publications, expected || name === "publication_failure" ? 1 : 0, name);
    if (expected) {
      const accepted = JSON.parse(fs.readFileSync(receipt(first), "utf8"));
      assert.equal(accepted.prompt_sha256, createHash("sha256").update(first.prompt).digest("hex"));
      assert.equal(accepted.prompt_utf8_bytes, Buffer.byteLength(first.prompt, "utf8"));
      if (name === "conversation") assert.equal(accepted.session_id, sessionId);
    }
  }
  assert.equal(aborts, 0, "acknowledgement grants no abort authority");
  assert.equal(fs.existsSync(join(directory, "cancel.json")), false);
  assert.deepEqual(fs.readdirSync(join(directory, "events")), []);
  assert.equal(fs.existsSync(join(directory, "pi-cancel-accepted.json")), false);
  assert.equal(JSON.parse(fs.readFileSync(join(directory, "status.json"))).state, "working");
  if (name !== "undelivered") assert.equal(payloads.length, 0, "acknowledgement publishes no result");
  await emit("session_shutdown");
  assert.equal(timers.size, 0);
  results.push({ name, receipts: records.map((record) => fs.existsSync(receipt(record))) });
}
fs.writeFileSync(join(root, "outputs.json"), JSON.stringify(results));
