import { report } from "./report.js";
import { deflect } from "./config.js";

// No person is at a managed session's dialog. The turn protocol's marker requirement is
// restated so that ending the turn early keeps the final line Bridge completes on.
const REASON = "Agent Bridge managed session: no person is at this question dialog, and the caller " +
  "cannot see it. Do not call AskUserQuestion again in this turn. End this turn now with the " +
  "question as your final response, in this form: QUESTION FOR CALLER: <question> Options: " +
  "<option labels>. The caller will send the answer as a follow-up message; continue from there. " +
  "Every requirement of the [Agent Bridge Claude turn protocol] in the request still applies to " +
  "this final response, including its exact marker on its own final line.";

// Question text and option labels in order: what the record retains, never the whole input.
function projection(questions) {
  try {
    return JSON.stringify(questions.map(question =>
      [question.question, question.options.map(option => option.label)]));
  } catch {
    return undefined;
  }
}

// Passes the stream through unchanged (yield* forwards next, return and throw to it) and
// notes each tool block's call id and its position among the response's tool uses.
function observed(stream, step) {
  return {
    [Symbol.asyncIterator]() { return this; },
    async next(value) {
      const item = await stream.next(value);
      const chunk = item.done ? undefined : item.value;
      if (chunk?.kind === "tool" && Number.isSafeInteger(chunk.index) &&
          !step.blocks.has(chunk.index) && step.blocks.size < 1024) {
        step.blocks.set(chunk.index, step.blocks.size);
        if (chunk.name === "AskUserQuestion" && typeof chunk.id === "string") {
          step.calls.set(chunk.id, step.blocks.get(chunk.index));
        }
      }
      return item;
    },
    return: value => stream.return(value),
    throw: error => stream.throw(error),
  };
}

// A refusal marks the retained emission only when the step's engine/core evidence holds the
// same projected questions at the refused call's position. Otherwise no item is named. The
// wire carries the refused questions: a repeated step can leave an earlier question retained
// at this position, and Bridge sets the flag only on an item with the same text and labels.
async function attribute($, step, refusal) {
  const evidence = step.keys.get(refusal.index);
  if (evidence !== undefined && evidence === refusal.key) {
    await report($, "question.deflected", { turnId: step.turnId, step: step.index,
      index: refusal.index, questions: refusal.questions }, step.conversation);
  } else {
    await report($, "question.emissions",
      { turnId: step.turnId, step: step.index, items: [], incomplete: true }, step.conversation);
  }
}

export function register(on) {
  // Optional observation has its own module: a policy refusing turn.step must not
  // remove the completion module's readiness or terminal-event hooks.
  const seen = new Map();
  // The latest main-loop step this module observes. Its tool calls run before the next step.
  let current;
  on("turn.complete", async ($, e, next) => {
    if (e.agentId === undefined) {
      seen.delete(e.turnId);
      if (current?.turnId === e.turnId) current = undefined;
    }
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
    // Claude can start a tool while its step still streams, before the response's evidence
    // exists: such a refusal is deferred and attributed after the emission report.
    const step = { conversation, turnId: e.turnId, index: e.index, blocks: new Map(),
      calls: new Map(), denied: new Set(), keys: new Map(), refused: [], deferred: true };
    current = step;
    let response;
    let streamed = false;
    try {
      response = yield* observed(next(e), step);
      streamed = true;
    } finally {
      if (!streamed) {
        // A stream that throws or is closed leaves no step evidence, so a call refused
        // meanwhile can name no item: each one marks the record incomplete.
        step.deferred = false;
        try {
          while (step.refused.length) await attribute($, step, step.refused.shift());
        } catch {}
      }
    }
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
      for (const item of items) step.keys.set(item.index, projection(item.questions));
      const incomplete = repeated || changed || (valid && !normal && core.toolUses.some(tool => tool.name === "AskUserQuestion"));
      if (items.length || incomplete) {
        await report($, "question.emissions", { turnId: e.turnId, step: e.index, items, incomplete }, conversation);
      }
    } catch {}
    // After the emission report, so that a deflection names an item the record already holds.
    // A call refused meanwhile joins this queue; once it is empty, later calls report alone.
    try {
      while (step.refused.length) await attribute($, step, step.refused.shift());
    } catch {}
    step.deferred = false;
    return response;
  });
  // Only a model's main-loop call whose streamed tool chunk this module passed is refused:
  // a subagent's call, a plugin's own call (`$.ui.ask`) and an unobserved call reach the
  // dialog as before. A hook that throws or overruns is skipped, so this fails open.
  on("tool.call", { tool: "AskUserQuestion" }, async ($, e, next) => {
    if (deflect !== true || e.agentId !== undefined || next.origin?.plugin !== "engine" ||
        next.origin?.tier !== "core") return next(e);
    const step = current;
    // A hook above Bridge can dispatch the same call again: same refusal, recorded once.
    if (step?.denied.has(e.tool_use_id)) return { deny: REASON };
    if (!step?.calls.has(e.tool_use_id)) return next(e);
    const refusal = { index: step.calls.get(e.tool_use_id), key: projection(e.questions),
      questions: e.questions };
    step.calls.delete(e.tool_use_id);
    step.denied.add(e.tool_use_id);
    if (step.deferred) step.refused.push(refusal);
    else await attribute($, step, refusal);
    return { deny: REASON };
  });
}
