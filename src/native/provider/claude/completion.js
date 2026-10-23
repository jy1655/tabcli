import { report } from "./report.js";

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
