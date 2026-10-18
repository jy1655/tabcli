# Codex Windows initial paste evidence (#124): Windows live verification

## Candidate and environment

Source: branch `fix/124-paste-evidence` on main `74f4d79` (v0.4.17): R1 `904cf23` (terminal
primitives), R2 `99bd07d` (adapter protocol, first form), R2c `7ef4095` (parser end-of-composer,
normalized merged count, 150 ms split gap, real-screen fixtures), R3 `df53d7d` (claim guard under
the lifecycle lock at every injected-route write, structural dialog and picker recognition), R4
`c7af321` (one addressed composer span shared by the parser, the overlay recognizer and the final
classification; the helper's attached dispatch as one production function), R5 `249a9a3` (a numbered
first composer row is a menu selection only with a pinned option label or companion row), R6
`657fe48` (every prompt glyph outside the addressed span and every pinned menu label or hint anywhere
is unresolved UI; the first prompt row starts the draft unless it is one), R7 `0aad896` (a
confirmation hint is recognized by its fragments on any row, so wrapped and keymap-spelled hints
count), R8 `38aa158` (any middle-dot part or row ending in an action verb after key tokens, or a
bare action word, is hint evidence; the recognizer's rendering boundary is documented). Observed on 2026-10-18 between
18:3x and 19:2x KST on native Windows 11 Home 10.0.26200 with Windows Terminal 1.24.12741.0
(dedicated `agent-bridge` window, 120 columns) and PowerShell 7.5.4; codex-cli 0.163.0 with a
ChatGPT account, default model `gpt-6-astra` at `xhigh`. The installed binary stayed 0.4.17.

Three candidates were run by their absolute path `C:\Users\user\.cargo\bin\tabcli-124.exe`: the
R2 build (`cargo build --release --locked` of `99bd07d`, SHA-256 `14a289c1…`), whose matrix
failed and produced the screens that are now fixtures; the R2c build (of `7ef4095`, SHA-256
`2ce2206a…`), whose results are the second matrix and the other checks below; the R3 build
(of `df53d7d`, SHA-256 `b2d9c911…`) for the three R3 rows; the R4 build (of `c7af321`, SHA-256
`6dc37479…`) for the three R4 rows; the R5 build (of `249a9a3`, SHA-256 `5c688c7b…`) for the
three R5 rows; the R6 build (of `657fe48`, SHA-256 `f33ecce7…`) for the three R6 rows; the R7 build (of
`0aad896`, SHA-256 `2c695ae1…`) for the three R7 rows; and the R8 build (of `38aa158`, SHA-256
`11b66349…`) for the two R8 rows. Every session was launched with
`--terminal windows-console --yolo`, unattended, with this checkout as the workspace, and closed
with `close-session --explicit` afterwards. `~/.codex/hooks.json` was changed only for the picker
check and restored byte-for-byte; `settings codex-lifecycle-hooks` was turned off only for the
legacy check and turned back on.

## Automated checks

- Each round's child ran the serial suite before the orchestrator committed: R1 1050 / 0 / 5,
  R2 1062 / 0 / 5, R2b 1067 / 0 / 5, R2c 1071 / 0 / 5 (passed / failed / ignored), with clippy
  on the host and for `x86_64-unknown-linux-gnu`, `cargo fmt -- --check` and `git diff --check`.
- Orchestrator re-runs after each round: `7ef4095` 1071 / 0 / 5, `df53d7d` 1075 / 0 / 5, `c7af321`
  1077 / 0 / 5, `249a9a3` 1078 / 0 / 5, `657fe48` 1079 / 0 / 5, `0aad896` 1080 / 0 / 5 and the R8
  tree committed as `38aa158` 1081 / 0 / 5 (serial `cargo test --all-targets --all-features
  --no-fail-fast -- --test-threads=1`), each with `cargo clippy --all-targets --all-features -- -D
  warnings` on the host and for `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`, `cargo fmt
  --all -- --check` and `git diff --check`.
- Windows CI: the pull request's run is named in the PR before merge.

## First matrix (R2 build): every prompt left in the composer

All six prompts ended `delivery-uncertain` after the 400 s deadline with
`paste observation: Err(Ambiguous("missing footer"))`. The parser required a footer row that
starts with `? for shortcuts` or ends with `% context left`; the real Codex 0.163.0 screen ends
the draft with the model and workspace row (`  GPT-6-Astra xhigh · D:\Dev\agent-bridge`) and a
right-aligned `⚠ 1 warning · f2 to view` row. The six screens, read with
`native-console-control screen`, are the fixtures under
`src/native/provider/codex/launch/paste/fixtures/`; they also showed:

| Prompt | Composer rows | Lesson |
| --- | --- | --- |
| short (framed body 237 scalars) | seven inline rows ending with the marker line | wrapped and blank rows are ordinary |
| body 999 and 1000 | one `› [Pasted Content 1060 chars]`, no marker row | the tail merged into the body burst (common, not rare) |
| body 1001, multi, 25 KB | `› [Pasted Content N chars]` then the marker row | the tail rendered separately |
| 25 KB and multi | N = 25595 and 1093 for bodies of 25596 and 1094 scalars | Bridge trims the source prompt before framing; Codex collapses nothing |

Nothing was submitted and no Return was written in any of the six sessions (the protocol never
reached the guarded submission); all six were closed.

## Second matrix (R2c build)

Every row completed through the injected protocol: binding observed, result published with
`provenance: codex_transcript_task_complete`, session back to `ready`, and the rollout's
`user.text` of the bound turn **byte-identical** to the framed prompt (header, trimmed source
prompt, metadata line and marker), checked by a script against Codex's own transcript.

| Case | Framed body (scalars) | Session | Bridge elapsed | Result |
| --- | --- | --- | --- | --- |
| short, one 104-char line | 237 | session-3jo4IW | 23.8 s | `R124-SHORT-OK` |
| body 999 (placeholder threshold −1) | 999 | session-U22Dvu | 23.7 s | `R124-OK` |
| body 1000 (threshold) | 1000 | session-WmIL5G | 19.0 s | `R124-OK` |
| body 1001 (threshold +1) | 1001 | session-kZktWT | 24.6 s | `R124-OK` |
| multi: Korean line wider than the console, emoji with ZWJ and skin tone, blank lines, `> ` lines, trailing spaces | 1094 | session-ZPK8e8 | 23.5 s | `R124-MULTI-OK` |
| 25 KB (the #127 reproduction prompt) | 25596 | session-8ZEEg7 | 127.4 s | `R5-A5-OK` and 120 lines |

Whether a given run rendered the marker separately or merged it into the placeholder is not
recorded by the protocol; the first matrix observed both shapes, and the fixtures pin both.

## Other live checks (R2c build)

| Check | Session | Observed |
| --- | --- | --- |
| console-window fallback (`wt.exe` hidden from `PATH`) | session-hDJefD | `surface=console-window`; completed through the injected protocol in 19.8 s |
| legacy route (`settings codex-lifecycle-hooks off`) | session-K5a1o1 | `codex-launch-mode.json` `not_injected` / `settings_off`; the old route (two delayed Returns, empty-composer wait) completed in 19.1 s |
| hook-review picker (untrusted user Stop hook in `hooks.json`) | session-5ktlGZ | `Hooks need review` on screen; no console input written; at the 70 s deadline `Codex empty composer readiness was not observed; no initial console input was sent. The last screen contained Review hooks or Trust all and continue (hook-review picker); Bridge never answers this picker.`; `status.json` `failed`; closed |
| Agy self-test (`--yolo`) | session-TH6PgX | passed in 28.4 s (Agy 1.3.3): ask, initial result, tell, follow-up, cleanup; the Agy paste path is unchanged by this branch |
| Pi self-test (`--model openai-codex/gpt-5.6-sol`) | session-tPkIfK | failed before any paste: Pi exited with code 1 about 1.5 s after spawn, after writing its startup receipt. Pi's own `crashes.json` records `TypeError: zlib.createZstdDecompress is not a function` (Pi 1.3.0 on Node 22.14.0); the installed 0.4.17 fails identically, so this is a Pi environment defect, not a branch regression |

## R3 to R8 builds: dialog words and numbered lists inside the prompt, and regressions

The final review found that the R2c parser vetoed any screen containing the words of the trust
dialog or the hook picker, so a prompt that mentions them could never be submitted; R3 recognizes
those views by their own rows instead. Each row below completed with the binding observed and the
rollout `user.text` byte-identical to the framed prompt.

| Case | Session | Bridge elapsed | Result |
| --- | --- | --- | --- |
| prompt quoting `Hooks need review`, `Review hooks`, `Trust all and continue`, `Trust this folder?`, `Trust and continue` (framed 494 scalars) | session-HsCCBd | 25.7 s | `R124-DIALOG-OK` |
| short (regression) | session-E9I89M | 25.4 s | `R124-SHORT-OK` |
| 25 KB (regression) | session-82LMDF | 115.5 s | `R5-A5-OK` and 120 lines |
| R4: the same dialog-words prompt | session-G64EjW | 24.8 s | `R124-DIALOG-OK` |
| R4: short (regression) | session-xJZVY1 | 26.9 s | `R124-SHORT-OK` |
| R4: 25 KB (regression) | session-6pLiKP | 135.4 s | `R5-A5-OK` and 120 lines |
| R5: a 24-line numbered list (`1. …` to `24. …`, framed 1889 scalars) | session-BOVYe0 | 23.2 s | `R124-NUMBERED-OK` |
| R5: the dialog-words prompt | session-7VDqpF | 27.4 s | `R124-DIALOG-OK` |
| R5: short (regression) | session-dfoBx4 | 25.4 s | `R124-SHORT-OK` |
| R6: the dialog-words prompt | session-tRkX7l | 26.3 s | `R124-DIALOG-OK` |
| R6: the 24-line numbered list | session-pyOvEL | 25.2 s | `R124-NUMBERED-OK` |
| R6: short (regression) | session-iCaO59 | 25.2 s | `R124-SHORT-OK` |
| R7: the dialog-words prompt | session-4cfo9X | 25.8 s | `R124-DIALOG-OK` |
| R7: the 24-line numbered list | session-kN8EwZ | 26.9 s | `R124-NUMBERED-OK` |
| R7: short (regression) | session-xKSDSW | 34.3 s | `R124-SHORT-OK` |
| R8: the dialog-words prompt | session-DVl1le | 25.9 s | `R124-DIALOG-OK` |
| R8: short (regression) | session-X68ze1 | 25.9 s | `R124-SHORT-OK` |

The claim guard of R3, the title-clipped picker and footer-shaped-content cases of R4 and the
scrolled inline draft whose first visible row is a numbered instruction (R5) and the menu suffixes,
wrapped third option and accepted numbered-label limitation of R6, the wrapped and
keymap-spelled confirmation hints of R7 and the chord and bare-action-word hints of R8, were
exercised by the deterministic seam and fixture tests only; no live interleaving or torn frame was forced, and the
live numbered list above was long enough to render as a placeholder rather than inline.

## Not verified here

- A screen that changes between the accepted read and the guarded Return (the `changed` path);
  a deliberately forced burst split beyond the two shapes the first matrix produced; a narrow or
  resized viewport; macOS (no console paste path).
- Whether a run used the marker path or the merged fallback (no record).
- Agy and Pi were smoke-tested only; their paste path did not change (facts and helper arguments
  are unchanged; the routing test proves it).
