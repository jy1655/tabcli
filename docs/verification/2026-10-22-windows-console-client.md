# Windows console client verification

Architecture card 2, [PR #278](https://github.com/jy1655/tabcli/pull/278),
source `84126f216a042274127dc2ba0a8ba5ee7768dc00`, based on `35ff600`.
The owned `Client<H: HelperProcess>` replaces production selection of a thread-local
test helper. The actual decoder/dispatcher test path and Windows input policy remain.
Card 3 was independently merged as #276 (`6d8f54e`).

## Automated checks

- Native Windows full serial suite: **1,199 passed, 0 failed, 5 ignored**.
- Console component suite: 15 passed. Codex's late-retirement helper test and consent's
  original-expiry helper test passed, including their retirement/phase matrices.
- Native, Linux and macOS all-target/all-feature Clippy with warnings denied, formatting,
  diff checks and `cargo build --release --locked` passed.
- [CI run 39781463208](https://github.com/jy1655/tabcli/actions/runs/39781463208):
  all six jobs passed. The Windows job ran **1,201 passed, 0 failed, 5 ignored** on
  merge `b1b2d3722b7654362f99d4febb441c23b8f6898c`, head `84126f2` with base `6d8f54e`.
  Its two additional tests came from card 3. Ignored tests are not runtime evidence.

## Authenticated Windows round trips

The locked release build reports 0.4.23, SHA-256
`20fae6b2cdca552535c6c2e4a834e2c08b624a3bb2e2f3d4a3f296695ef04f3f`.
The installed release remains 0.4.22. Each run used `self-test <provider> --workspace
D:/Dev/agent-bridge --terminal windows-console --json`, the ordinary state root and
existing workspace trust, without `--yolo` or permission-setting changes. The npm
directory was appended to that process's PATH to expose Codex without shadowing Claude.

| Provider | Result | Owned session |
| --- | --- | --- |
| Codex 0.163.0 | All five steps passed; 27.730 s | `session-DSBbgo`, closed |
| Claude 2.1.308 | All five steps passed; 23.394 s | `session-nJrp6U`, closed |
| Pi 1.3.0 | All five steps passed; 15.338 s | `session-QW9Phw`, closed |
| Agy 1.4.5 | Initial input received; result timed out awaiting tool approval | `session-62jd8W`, closed |

Agy's own `agy.log` recorded `HandleUserInput` at 21:51:15.999 KST and
`Forwarding user message` at 21:51:18.626, then a `RunCommand` confirmation at
21:51:25.206. The self-test needed no tools, but the provider requested one. Bridge's
initial-result wait timed out after 120.011 s; tell and its result were not verified.
Cleanup passed. No approval was answered or bypassed, and no prompt was retried.
This is a partial Agy run, not a passing round trip.

All four task-owned sessions were closed; unrelated sessions were preserved.
The successful round trips do not prove a new trust-dialog approval, and do not cover
#260's unmerged Claude native completion path. Client tests model helper/Win32 effects;
actual attachment is evidenced only by the live runs above.

Private test, Clippy and CI logs are `.git/card2-*.log`; runtime JSON and stderr are
`.git/card2-live/`. This report is on the Windows verification branch so evidence-only
updates do not replace the implementation commit that passed CI.
