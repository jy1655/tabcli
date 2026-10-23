import { report } from "./report.js";

export function register(on) {
  // Optional observation has its own module: a policy refusing turn.step must not
  // remove the completion module's readiness or terminal-event hooks.
  const seen = new Map();
  on("turn.complete", async ($, e, next) => {
    if (e.agentId === undefined) seen.delete(e.turnId);
    return next(e);
  });
  on("turn.step", async function* ($, e, next) {
    if (e.agentId !== undefined || next.origin?.plugin !== "engine" || next.origin?.tier !== "core" ||
        !Number.isSafeInteger(e.index) || e.index < 0) return yield* next(e);
    let conversation;
    try { conversation = await $.session.id(); } catch { return yield* next(e); }
    let steps = seen.get(e.turnId);
    if (!steps && seen.size < 16) { steps = new Set(); seen.set(e.turnId, steps); }
    const repeated = !steps || steps.has(e.index) || steps.size >= 1024;
    if (steps && steps.size < 1024) steps.add(e.index);
    const response = yield* next(e);
    try {
      // A lower hook may synthesize its own return. Only the engine's final trace
      // entry supplies model evidence; an absent trace supplies none.
      const trace = next.trace;
      const last = trace?.at(-1);
      const core = last?.plugin === "engine" && last?.tier === "core" ? last.returned : undefined;
      const valid = core?.turnId === e.turnId && core?.index === e.index && Array.isArray(core?.toolUses);
      const normal = valid && core.stopReason === "tool_use";
      const changed = valid && JSON.stringify(core.toolUses) !== JSON.stringify(response?.toolUses);
      const items = normal ? core.toolUses.flatMap((tool, index) =>
        tool.name === "AskUserQuestion" ? [{ index, questions: tool.input?.questions }] : []) : [];
      const incomplete = repeated || changed || (valid && !normal && core.toolUses.some(tool => tool.name === "AskUserQuestion"));
      if (items.length || incomplete) {
        await report($, "question.emissions", { turnId: e.turnId, step: e.index, items, incomplete }, conversation);
      }
    } catch {}
    return response;
  });
}
