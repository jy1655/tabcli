const MAX_WIRE_BYTES = 4 * 1024 * 1024;
const encoder = new TextEncoder();

async function report($, event, data, observedConversation) {
  try {
    const executable = await $.env.get("AGENT_BRIDGE_EXECUTABLE");
    if (!executable) return;
    const conversation = observedConversation ?? await $.session.id();
    let stdin = JSON.stringify({ agent_bridge_claude_mod: 1, event, conversation, data });
    const utf8_len = encoder.encode(stdin).length;
    if (utf8_len > MAX_WIRE_BYTES) {
      if (event === "question.emissions") {
        stdin = JSON.stringify({ agent_bridge_claude_mod: 1, event, conversation,
          data: { turnId: data.turnId, step: data.step, items: [], incomplete: true } });
      } else {
        if (event !== "turn.complete") return;
        stdin = JSON.stringify({ agent_bridge_claude_mod: 1, event: "oversize", conversation,
          data: { turnId: data.turnId, utf8_len } });
      }
    }
    for (let attempt = 0; attempt < 2; attempt++) {
      try {
        const result = await $.process.run([executable, "native-hook", "claude"], { stdin, timeoutMs: 30000 });
        if (result.exitCode === 0) return;
      } catch {}
    }
    $.ui.log(`Agent Bridge could not record Claude ${event}; inspect the session before sending again.`);
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
  // The completed model step owns turn identity; tool.call and classic hooks do not.
  // Pass every streamed chunk and the final response through unchanged. An emission
  // says nothing about whether the tool ran, is still asking, or received an answer.
  on("turn.step", async function* ($, e, next) {
    if (e.agentId !== undefined || next.origin?.plugin !== "engine" || next.origin?.tier !== "core") {
      return yield* next(e);
    }
    let conversation;
    try { conversation = await $.session.id(); } catch { return yield* next(e); }
    const response = yield* next(e);
    try {
      if (response.turnId !== e.turnId || response.index !== e.index ||
          !Number.isSafeInteger(e.index) || e.index < 0 || !Array.isArray(response.toolUses)) return response;
      const items = response.toolUses.flatMap((tool, index) =>
        tool.name === "AskUserQuestion" ? [{ index, questions: tool.input?.questions }] : []);
      if (items.length) {
        await report($, "question.emissions", { turnId: e.turnId, step: e.index, items, incomplete: false }, conversation);
      }
    } catch {}
    return response;
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
