const MAX_WIRE_BYTES = 4 * 1024 * 1024;
const encoder = new TextEncoder();

async function report($, event, data) {
  try {
    const executable = await $.env.get("AGENT_BRIDGE_EXECUTABLE");
    if (!executable) return;
    const conversation = await $.session.id();
    let stdin = JSON.stringify({ agent_bridge_claude_mod: 1, event, conversation, data });
    const utf8_len = encoder.encode(stdin).length;
    if (utf8_len > MAX_WIRE_BYTES) {
      if (event !== "turn.complete") return;
      stdin = JSON.stringify({ agent_bridge_claude_mod: 1, event: "oversize", conversation,
        data: { turnId: data.turnId, utf8_len } });
    }
    for (let attempt = 0; attempt < 2; attempt++) {
      try {
        const result = await $.process.run([executable, "native-hook", "claude"], { stdin, timeoutMs: 30000 });
        if (result.exitCode === 0) return;
      } catch {}
    }
    $.ui.log("Agent Bridge could not record the Claude completion signal; inspect the pending request before sending again.");
  } catch {}
}

export function register(on) {
  on("session.start", async ($, e, next) => {
    if (e.agentId === undefined) await report($, "session.start", {});
    return next(e);
  });
  on("turn.start", async ($, e, next) => {
    if (e.agentId === undefined) await report($, "turn.start", { turnId: e.turnId, text: e.text });
    return next(e);
  });
  on("turn.complete", async ($, e, next) => {
    if (e.agentId === undefined) {
      const data = { turnId: e.turnId, reason: e.reason, isAborted: e.isAborted, answer: e.answer };
      if (e.refusal !== undefined) data.refusal = e.refusal;
      await report($, "turn.complete", data);
    }
    return next(e);
  });
}
