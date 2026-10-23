# Question handoff, 2026-10-24

This completes the notification and ordinary-follow-up shape proposed in #189, building on
Codex observation (#262), question-aware wait (#303), and Claude observation (#304). A parent
can observe a structured question before completion, notice retained questions on ordinary
completion outputs, retrieve the question, and supply missing information through a new
`tell` once the session is Ready. It is not an addressed answer to a pending provider dialog.

## Observable contract

The query layer adds `question_emitted: true` and `question_emission_count` when the selected
Request retains emissions. The count is of emission items, each of which may contain multiple
questions. An absent field remains unknown. Existing incomplete/unreadable observations are
preserved; neither the count nor the presence flag claims that a question is shown, pending,
answered, or unresolved.

Ordinary `result`, `inspect`, and the Request focused by `status` use the same retained evidence.
Waited successful `ask`, `tell` and `reopen`, `result --wait`, and default `wait` now expose the
count after completion. Plain output directs the parent to the selected Request's `result`
for question text. A pending default wait does not read question records or wake on them.
`wait --questions` retains its opt-in wake, suffix and cursor behavior. Closed status results
can carry historical presence without an active attention flag. Result lists, timeline,
attachments, Claim transitions, completion records and delivery transports are unchanged.

The integration guidance describes the parent loop and checks help for `--questions` on older
compatible binaries. The parent reads the actual question before accepting a result that may
have continued under assumptions. A `tell` remains a new Request and writes no answer receipt.

## Automated checks

At `a88a5d4d422c7115eef6110f71d5da8b7554f60a`, the full serial all-target/all-feature suite
passed on native Windows: 1,308 passed, 5 ignored, none failed. It took 184.01s including
incremental compilation, with the #305 performance change included. Host Clippy, all-feature
Linux and Windows cross-target Clippy, rustfmt, diff and language checks passed. The shared
integration skill was regenerated for all four hosts; generation/check and Python package
tests passed.

New tests exercise real Claim, Report, Receipt and Snapshot records through completed query
outputs and the actual Turn runner's plain/JSON output. They check read-only behavior,
request selection, retained counts, closed history, unchanged pending default waits, and
an unreadable optional question record that does not change successful completion. Existing
Codex fixtures retain the exact-byte exclusions for list, timeline and attachments. These
tests do not establish provider question resolution or authenticated macOS behavior.

## Native Windows Codex round trip

The debug candidate was built with Cargo from the commit above, copied outside `target/`
before launch, and used for every ask, query, tell and close. Its version was `tabcli 0.4.25`
and SHA-256 was `1449c336eaa3f76fd159a1eebaebc7efd7a26f6271cb0171c352e01ef86756ed`.
Provider: codex-cli 0.164.0, provider-default model/effort. Surface: managed native Windows
console. Overrides: `--terminal windows-console --yolo`; ordinary state root, UTF-8 prompt
files, no trust-store or provider-setting edits. The caller was Codex on Windows/PowerShell.

Session `session-fGL8HD`:

1. Request `request-1792770823941162000-14100-0` deliberately lacked a baseline code. The prompt
   required a structured question, a 20-second local sleep to expose the observation interval,
   then the truthful blocked-work report `NEED_BASELINE_CODE` with the normal Bridge suffix.
2. `wait ADDRESS --questions --timeout-secs 55 --json` returned `What is the baseline code?`
   while the Request was `pending`, with `ended: null`, count 1, and the address still in
   `remaining`. No answer, Claim release or completion was inferred from this emission.
3. The Request completed with exactly `NEED_BASELINE_CODE`, count 1, and the same retained
   emission. Default `wait` and `result --wait` exposed the count without question content.
   The cursor query ran after completion: it returned the ending and no repeated question.
   This run therefore does not claim a cursor timeout before completion; #303 records that
   separate runtime check.
4. Once Ready, `tell --prompt-file` supplied `ORBIT-305`. The distinct Request
   `request-1792770922370772900-27924-0` completed with exactly `BASELINE=ORBIT-305` and did not
   inherit the first Request's question fields. Both results named the same provider conversation
   and distinct turns.
5. The first Request still retained the original question after the follow-up. Explicit close
   reported `ok: true`, `closed: true`, no open descendants and a complete scan. A post-close
   result and inspect retained the question and original blocked-work report.

The code was supplied through a later ordinary Request; the provider's original async emission
has no Bridge-owned answered state. The test deliberately prompted tool use and a bounded
blocked-work ending. It does not prove unsolicited tool use, automatic ending of every
blocked task, or an external steering path into an active Codex TUI turn.

## Provider limits

Codex's available async question tool can emit and return to the model. The current native
queue transports a later turn when idle; this change does not add an addressed external
same-turn reply endpoint to the embedded TUI.

Claude's retained `AskUserQuestion` observation is covered by the separate
[PR #304 record](2026-10-23-claude-question-emissions.md). An emission alone does not prove a
dialog is open. When the actual surface does show a pending question, the person can answer
there; Bridge supplies neither an answer nor a permission override. Claude 2.1.309's installed
settings schema describes `askUserQuestionTimeout` as opt-in, defaulting to `never`; this task
did not change that setting or use an internal timeout environment override. No automated
Claude question-answer round trip, dialog auto-resolution, or current-candidate authenticated
macOS question run is claimed.

Agy and Pi have no structured question observer in this implementation. No prose heuristic,
common fallback input transport, new question tool or reverse message route is introduced.
The supported completion criterion is explicit question visibility and a normal continuation
when the provider has ended the Request, demonstrated for the original Codex use case.
