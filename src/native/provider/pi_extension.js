import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { closeSync, fstatSync, lstatSync, openSync, readFileSync, readSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

function assistantText(message) {
  if (!Array.isArray(message.content)) return undefined;
  const text = message.content
    .filter((part) => part?.type === "text" && typeof part.text === "string")
    .map((part) => part.text)
    .join("\n")
    .trim();
  return text || undefined;
}

function lastAssistantOutcome(messages) {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "assistant") continue;
    if (message.stopReason !== "stop") {
      const detail = typeof message.errorMessage === "string" && message.errorMessage.trim()
        ? `: ${message.errorMessage.trim()}`
        : "";
      return { agent_bridge_error: `Pi turn ended with ${message.stopReason ?? "an unknown state"}${detail}` };
    }
    const text = assistantText(message);
    if (text) return { last_assistant_message: text };
    return { agent_bridge_error: "Pi settled without assistant text." };
  }
  return { agent_bridge_error: "Pi settled without an assistant result." };
}

// Pi documents `usage` as a required field of every assistant message (docs/message-types.md,
// "Assistant messages always contain usage"; `Usage` in @earendil-works/pi-ai types.d.ts). The
// accumulator sums the usage of the assistant messages that every `agent_end` of the correlated
// claim delivers; Pi's agent loop emits only the run's new messages there (pi v1.2.0
// packages/agent/src/agent-loop.ts), so no deduplication is needed. Tool-result usage (nested
// model work), compaction summaries and Pi's computed `cost` are not collected.
const USAGE_FIELDS = ["input", "output", "cacheRead", "cacheWrite", "totalTokens"];
const USAGE_ERROR_LIMIT = 200;

function newUsage() {
  return {
    runs: 0,
    assistantMessages: 0,
    sums: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, reasoning: 0 },
    reasoningEverywhere: true,
    complete: true,
    error: undefined,
  };
}

function countable(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

function addUsageField(accumulator, usage, field) {
  if (accumulator.error) return;
  const value = usage[field];
  if (!countable(value)) {
    accumulator.error = `assistant message usage.${field} is not a non-negative safe integer`;
    return;
  }
  const sum = accumulator.sums[field] + value;
  if (!Number.isSafeInteger(sum)) {
    accumulator.error = `usage.${field} sum exceeds the safe integer range`;
    return;
  }
  accumulator.sums[field] = sum;
}

function accumulateRun(accumulator, messages) {
  if (!accumulator) return;
  accumulator.runs += 1;
  const assistants = Array.isArray(messages)
    ? messages.filter((message) => message?.role === "assistant")
    : [];
  // Pi emits a synthetic failure message with zero usage after an escaped exception, and
  // earlier calls of the run may be missing; only an ordinary stop proves the run complete.
  if (assistants.length === 0 || assistants.at(-1).stopReason !== "stop") accumulator.complete = false;
  for (const message of assistants) {
    accumulator.assistantMessages += 1;
    if (accumulator.error) continue;
    const usage = message.usage;
    if (usage === null || typeof usage !== "object") {
      accumulator.error = "assistant message has no usage";
      continue;
    }
    for (const field of USAGE_FIELDS) addUsageField(accumulator, usage, field);
    if (usage.reasoning === undefined) {
      accumulator.reasoningEverywhere = false;
    } else {
      addUsageField(accumulator, usage, "reasoning");
    }
  }
}

function usagePayload(accumulator) {
  if (!accumulator) return {};
  if (accumulator.error) return { usage_error: accumulator.error.slice(0, USAGE_ERROR_LIMIT) };
  const sums = accumulator.sums;
  const usage = {
    coverage: accumulator.complete ? "complete" : "uncertain",
    runs: accumulator.runs,
    assistant_messages: accumulator.assistantMessages,
    input_tokens: sums.input,
    output_tokens: sums.output,
    cache_read_tokens: sums.cacheRead,
    cache_write_tokens: sums.cacheWrite,
    total_tokens: sums.totalTokens,
  };
  // A partial reasoning sum would misrepresent a subset of `output`.
  if (accumulator.reasoningEverywhere && accumulator.assistantMessages > 0) {
    usage.reasoning_tokens = sums.reasoning;
  }
  return { usage };
}

function wait(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function deliverResult(executable, payload) {
  if (!executable) {
    return { ok: false, detail: "Agent Bridge executable is unavailable" };
  }
  let detail = "native hook failed";
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const result = spawnSync(executable, ["native-hook", "pi"], {
      input: JSON.stringify(payload),
      encoding: "utf8",
      stdio: ["pipe", "ignore", "pipe"],
      timeout: 10000,
    });
    if (!result.error && result.status === 0) return { ok: true };
    const stderr = typeof result.stderr === "string" ? result.stderr.trim() : "";
    detail = result.error?.message || stderr || `native hook exited with status ${result.status}`;
    if (attempt < 2) await wait(100 * (attempt + 1));
  }
  return { ok: false, detail: detail.slice(0, 1024) };
}

function persistHookFailure(payload, detail) {
  const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
  if (!directory) return false;
  const target = join(directory, "pi-hook-failure.json");
  const temporary = join(
    directory,
    `.pi-hook-failure-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}.tmp`,
  );
  const signal = {
    error: `Pi result delivery failed: ${detail}`,
    provider_session_id: payload.session_id,
    turn_id: payload.turn_id,
    claim_token: payload.agent_bridge_claim_token,
  };
  if (payload.usage !== undefined) signal.usage = payload.usage;
  if (payload.usage_error !== undefined) signal.usage_error = payload.usage_error;
  try {
    writeFileSync(temporary, JSON.stringify(signal), { encoding: "utf8", flag: "wx", mode: 0o600 });
    renameSync(temporary, target);
    return true;
  } catch {
    try { unlinkSync(temporary); } catch {}
    return false;
  }
}

export default function (pi) {
  let pending;
  let undelivered = false;
  let activeClaimToken;
  let activePromptCorrelated = false;
  let cancelTimer;
  let runId = 0;
  let abortClaimToken;
  let usage;
  let inputCandidate;
  let inputAttemptedClaim;

  function readInputPending() {
    const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
    if (!directory) return undefined;
    let fd;
    try {
      const path = join(directory, "pi-pending-turn.json");
      if (!lstatSync(path).isFile()) return undefined;
      fd = openSync(path, "r");
      const stat = fstatSync(fd);
      if (!stat.isFile() || stat.size > 4096) return undefined;
      const bytes = Buffer.alloc(4097);
      let length = 0;
      while (length < bytes.length) {
        const count = readSync(fd, bytes, length, bytes.length - length, length);
        if (count === 0) break;
        length += count;
      }
      if (length > 4096) return undefined;
      return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes.subarray(0, length)));
    } catch {
      return undefined;
    } finally {
      if (fd !== undefined) { try { closeSync(fd); } catch {} }
    }
  }

  function armInput(event) {
    if (process.platform !== "win32"
        || (event.images !== undefined && (!Array.isArray(event.images) || event.images.length !== 0))) return;
    const binding = readInputPending();
    const claim = binding?.claim_token;
    const fingerprint = binding?.windows_input;
    if (binding?.schema !== 1 || typeof claim !== "string" || !/^[0-9-]{1,160}$/.test(claim)
        || binding.marker !== `<!-- agent-bridge-pi-turn:${claim} -->`
        || !fingerprint || Object.keys(fingerprint).length !== 2
        || typeof fingerprint.sha256 !== "string" || !/^[0-9a-f]{64}$/.test(fingerprint.sha256)
        || !Number.isSafeInteger(fingerprint.utf8_bytes) || fingerprint.utf8_bytes <= 0) return;
    const digest = createHash("sha256").update(event.prompt, "utf8").digest("hex");
    const length = Buffer.byteLength(event.prompt, "utf8");
    if (digest === fingerprint.sha256 && length === fingerprint.utf8_bytes) {
      inputCandidate = { claim_token: claim, prompt_sha256: digest, prompt_utf8_bytes: length };
    }
  }

  function publishInput(ctx) {
    const candidate = inputCandidate;
    inputCandidate = undefined;
    if (!candidate || inputAttemptedClaim === candidate.claim_token
        || readActiveClaimToken() !== candidate.claim_token) return;
    // This claim read witnesses a submitted run. Later retirement cannot undo it.
    inputAttemptedClaim = candidate.claim_token;
    const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
    const temporary = join(directory, `.pi-input-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}.tmp`);
    try {
      const session = ctx.sessionManager.getSessionId();
      if (typeof session !== "string" || !session.trim() || Buffer.byteLength(session, "utf8") > 256
          || /\p{Cc}/u.test(session)) throw new Error("invalid Pi session identity");
      writeFileSync(temporary, JSON.stringify({
        schema: 1, signal: "agent_start", ...candidate, session_id: session,
      }), { encoding: "utf8", flag: "wx", mode: 0o600 });
      renameSync(temporary, join(directory, `pi-input-accepted-${candidate.claim_token}.json`));
    } catch {
      try { unlinkSync(temporary); } catch {}
      ctx.ui.notify("Agent Bridge could not record Pi input submission. Delivery may remain uncertain; do not resend.", "warning");
    }
  }

  function stopCancelTimer() {
    // Invalidate callbacks before clearing the timer or changing run correlation.
    runId += 1;
    if (cancelTimer !== undefined) clearInterval(cancelTimer);
    cancelTimer = undefined;
  }

  function readRecord(name) {
    const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
    if (!directory) return undefined;
    try { return JSON.parse(readFileSync(join(directory, name), "utf8")); }
    catch { return undefined; }
  }

  function startCancelTimer(ctx) {
    const thisRun = runId;
    const claimToken = activeClaimToken;
    if (!claimToken || !activePromptCorrelated) return;
    // Pi v1.0.4 ctx is runtime-scoped, not a turn handle. This synchronous,
    // non-overlapping callback must check both run lifetime and current claim.
    cancelTimer = setInterval(() => {
      if (thisRun !== runId || claimToken !== activeClaimToken
          || !activePromptCorrelated || abortClaimToken === claimToken
          || readActiveClaimToken() !== claimToken) return;
      const cancel = readRecord("cancel.json");
      if (cancel?.schema !== 1 || cancel.claim_token !== claimToken) return;
      // Calling abort and publishing acceptance cannot be atomic. Never retry
      // this claim after a thrown abort or a failed publication.
      abortClaimToken = claimToken;
      const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
      const temporary = join(directory, `.pi-cancel-${process.pid}-${Date.now()}.tmp`);
      try {
        ctx.abort();
        writeFileSync(temporary, JSON.stringify({ schema: 1, claim_token: claimToken }),
          { encoding: "utf8", flag: "wx", mode: 0o600 });
        renameSync(temporary, join(directory, "pi-cancel-accepted.json"));
      } catch {
        try { unlinkSync(temporary); } catch {}
      }
    }, 200);
  }

  function readActiveClaimToken() {
    const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
    if (!directory) return undefined;
    try {
      const token = readFileSync(join(directory, "turn.claim"), "utf8").trim();
      return token || undefined;
    } catch {
      return undefined;
    }
  }

  // Official session_start follows project_trust resolution, including a user's decline.
  // Never return a trust decision here or persist one in Pi's provider-owned store.
  pi.on("session_start", (event, ctx) => {
    inputCandidate = undefined;
    stopCancelTimer();
    if (event.reason !== "startup") return;
    const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
    const claimToken = readActiveClaimToken();
    if (!directory || !claimToken) return;
    const temporary = join(directory, `.pi-startup-${process.pid}-${Date.now()}.tmp`);
    try {
      writeFileSync(temporary, JSON.stringify({
        schema: 2,
        claim_token: claimToken,
        session_id: ctx.sessionManager.getSessionId(),
        ...(process.platform === "win32" ? { input_ack_schema: 1 } : {}),
      }), { encoding: "utf8", flag: "wx", mode: 0o600 });
      renameSync(temporary, join(directory, "pi-startup-ready.json"));
    } catch {
      try { unlinkSync(temporary); } catch {}
      if (process.platform === "win32") {
        ctx.ui.notify("Agent Bridge could not confirm Pi startup; the initial console input will remain withheld.", "warning");
      }
    }
  });

  pi.on("before_agent_start", (event) => {
    inputCandidate = undefined;
    activePromptCorrelated = false;
    usage = undefined;
    stopCancelTimer();
    activeClaimToken = readActiveClaimToken();
    const userPrompt = typeof event?.prompt === "string"
      && (event.messageType === undefined || event.messageType === "user");
    activePromptCorrelated = Boolean(
      userPrompt && activeClaimToken
      && event.prompt.includes(`<!-- agent-bridge-pi-turn:${activeClaimToken} -->`),
    );
    // A new correlated prompt, or a replaced claim, starts the usage of that claim afresh;
    // a retry's agent_start without this event keeps accumulating.
    usage = activePromptCorrelated ? newUsage() : undefined;
    if (userPrompt) armInput(event);
  });

  pi.on("agent_start", (_event, ctx) => {
    stopCancelTimer();
    try { publishInput(ctx); } catch {}
    if (undelivered && pending) {
      if (!persistHookFailure(pending, "a prior result remained undelivered")) {
        ctx.ui.notify("Agent Bridge still cannot recover the previous Pi result.", "warning");
        return;
      }
      undelivered = false;
    }
    pending = undefined;
    startCancelTimer(ctx);
  });

  pi.on("agent_end", (event, ctx) => {
    stopCancelTimer();
    if (undelivered) return;
    if (!activePromptCorrelated || !activeClaimToken) {
      pending = undefined;
      usage = undefined;
      return;
    }
    accumulateRun(usage, event.messages);
    pending = {
      ...lastAssistantOutcome(event.messages),
      ...usagePayload(usage),
      session_id: ctx.sessionManager.getSessionId(),
      turn_id: ctx.sessionManager.getLeafId() ?? undefined,
      agent_bridge_claim_token: activeClaimToken,
      agent_bridge_prompt_correlated: true,
    };
    const assistant = event.messages.findLast((message) => message?.role === "assistant");
    const accepted = readRecord("pi-cancel-accepted.json");
    // Pi 1.0.4, observed 2026-10-11: abort during a tool can surface Node's
    // AbortError message as errorMessage with stopReason "error". Match that
    // exact message, never arbitrary errors containing "abort". agent-loop.ts
    // forwards the stream's final assistant outcome without normalizing it.
    const aborted = assistant?.stopReason === "aborted"
      || (assistant?.stopReason === "error" && assistant.errorMessage === "This operation was aborted");
    if (aborted && accepted?.schema === 1
        && accepted.claim_token === activeClaimToken) {
      pending.agent_bridge_cancel_claim_token = activeClaimToken;
    }
  });

  pi.on("agent_settled", async (_event, ctx) => {
    inputCandidate = undefined;
    stopCancelTimer();
    if (!pending) return;
    const payload = pending;
    const executable = process.env.AGENT_BRIDGE_EXECUTABLE;
    const delivery = await deliverResult(executable, payload);
    if (!delivery.ok) {
      if (persistHookFailure(payload, delivery.detail)) {
        pending = undefined;
        usage = undefined;
        ctx.ui.notify("Agent Bridge marked this undelivered Pi result as failed.", "warning");
        return;
      }
      undelivered = true;
      ctx.ui.notify("Agent Bridge could not record or recover this Pi result.", "warning");
      return;
    }
    pending = undefined;
    usage = undefined;
  });
  pi.on("session_shutdown", () => {
    inputCandidate = undefined;
    stopCancelTimer();
    activeClaimToken = undefined;
    activePromptCorrelated = false;
    usage = undefined;
  });
}
