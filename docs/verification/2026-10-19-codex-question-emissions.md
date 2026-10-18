# Codex question-emission candidate

This implements the observation-only Codex slice of #189. It does not close the broader
question, response, or parent-wake issue.

The compiling regression was committed at `8af1d36` before production changes. A real
Claim and Receipt, pending frame, and `UserPromptSubmit` hook bind a synthetic rollout.
The existing Observer then polls accepted user input and one async question item, without
tool output or a terminal. On the parent source, `result --request` had a null title where
the test expected the structured title. The candidate exposes it while the request stays
pending, together with status attention and inspect presence.

Focused synthetic tests cover acceptance, optional attribution, terminal boundaries, id
conflicts, the full 64 KiB record limit, publication races, replay, before-rename and
after-rename failures, corrupt records, read-only queries, and excluded query contracts.
They use synthetic strings and identities. They are not authenticated provider verification.

The implementation owner's focused command results and exact commits are in the local
`evidence/189-implementation/report.md` program artifact. Required root gates remain pending:

- The full serial all-target suite and cross-platform checks.
- Windows CI, including the record-sharing and rename cases.
- Independent Opus correctness and comment reviews.
- Actual parent and final-head native Windows Codex runs with `--yolo`, recorded binary
  hashes, free-text and options-bearing questions, and a bounded work barrier.
- Public result and status observation before terminal completion, unchanged ordinary
  completion, retained historical emission, and a Ready `tell` carrying a known fact.

The implementation owner ran no provider session for this candidate. Legacy history,
unprompted tool use, macOS runtime, other-provider questions, and every addressed answer
or resolution route remain unverified.
