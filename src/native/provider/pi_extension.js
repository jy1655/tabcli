import { spawnSync } from "node:child_process";
import { readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
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
    stopCancelTimer();
    activeClaimToken = readActiveClaimToken();
    activePromptCorrelated = Boolean(
      activeClaimToken
      && event.prompt.includes(`<!-- agent-bridge-pi-turn:${activeClaimToken} -->`),
    );
  });

  pi.on("agent_start", (_event, ctx) => {
    stopCancelTimer();
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
      return;
    }
    pending = {
      ...lastAssistantOutcome(event.messages),
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
    stopCancelTimer();
    if (!pending) return;
    const payload = pending;
    const executable = process.env.AGENT_BRIDGE_EXECUTABLE;
    const delivery = await deliverResult(executable, payload);
    if (!delivery.ok) {
      if (persistHookFailure(payload, delivery.detail)) {
        pending = undefined;
        ctx.ui.notify("Agent Bridge marked this undelivered Pi result as failed.", "warning");
        return;
      }
      undelivered = true;
      ctx.ui.notify("Agent Bridge could not record or recover this Pi result.", "warning");
      return;
    }
    pending = undefined;
  });
  pi.on("session_shutdown", () => {
    stopCancelTimer();
    activeClaimToken = undefined;
    activePromptCorrelated = false;
  });
}
