const MAX_WIRE_BYTES = 4 * 1024 * 1024;
const encoder = new TextEncoder();

export async function report($, event, data, observedConversation) {
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
    const timeoutMs = event === "question.emissions" ? 5000 : 30000;
    for (let attempt = 0; attempt < 2; attempt++) {
      try {
        const result = await $.process.run([executable, "native-hook", "claude"], { stdin, timeoutMs });
        if (result.exitCode === 0) return;
      } catch {}
    }
    $.ui.log(event === "question.emissions"
      ? "Agent Bridge could not retain this question observation; ordinary tool execution continues."
      : `Agent Bridge could not record Claude ${event}; inspect the session before sending again.`);
  } catch {}
}
