# Linux Herdr Core verification, 2026-11-18

The Linux debug build based on v0.4.45 ran against hc 0.1.21 on Ubuntu 26.04
x86_64. The test launched a separate, task-owned core with a private socket and
state directory. Each provider used its normal workspace-trust controls.

| Provider | Linux Herdr result |
| --- | --- |
| Claude Code 2.1.327, Opus | Explicit selection and automatic selection from inside an hc terminal each passed ask, initial result, tell, follow-up result and close. |
| Codex 0.170.0 | All five self-test steps passed, including the native queue follow-up. |
| Pi 1.5.2 | All five self-test steps passed, including the addressed Herdr input. |
| Agy 1.5.9 | The repository required workspace trust. Bridge withheld input and cleanup passed. No completed Agy turn was observed. |

The new Linux registration test failed on v0.4.45 with
`Herdr is not available on Linux; use tmux` and passed with the adapter registered.
The shared adapter tests also ran on Linux, including an actual Unix socket,
scripted lost creation and input replies, changed program identity, and repeated
close. The tests used the Linux boot ID and procfs start ticks for the program's
incarnation. They do not exercise a GUI, WSL, or remote hc machines.

## Request framing and completion (#396)

For the external prompt `Reply READY.`, with the same marker in both builds,
the UTF-8 sizes of the Linux provider message are:

| Provider | v0.4.45 | Candidate | Bytes removed per request |
| --- | ---: | ---: | ---: |
| Claude | 297 | 113 | 184 |
| Codex | 184 | 120 | 64 |
| Agy | 277 | 129 | 148 |
| Pi | 183 | 49 | 134 |

The Claude example uses `claude-turn-example` as the request ID. The other
examples use the claim token `1-2-3`. The reduction is independent of token
length. Claude's v0.4.45 message was:

```text
[Agent Bridge native delegation]
Source: external

Reply READY.

[Agent Bridge Claude turn protocol]
Complete this request as one turn. End the complete final response with the exact marker below on its own final line; do not alter or omit it.
<!-- agent-bridge-claude-turn:claude-turn-example -->
```

The same request in a new Claude session is:

```text
Reply READY.

End your final response with this exact line:
<!-- agent-bridge-claude-turn:claude-turn-example -->
```

Claude, Codex and Pi each passed the five steps again with the shorter frames.
Each run had two request receipts recording `source: external`. A separate
screen read returned the bound Claude terminal. A stale hc terminal ID refused
automatic selection before any session was created. The isolated core reported
zero live and zero finished terminals before shutdown.

A separate compatibility check launched Claude on tmux with the installed
v0.4.45 binary and used the candidate for the follow-up. Before the fix the
old hook rejected the compact pending frame and the request timed out. With
the launch-specific prefix restored, the same two-binary check returned
`FOLLOWOK` and closed. The focused old-hook regression test also failed
before the compatibility fix and passed afterward.

The combined Linux Rust suite passed 1,622 tests, with one ignored. Clippy passed
on Linux and compiled all targets for macOS arm64 and Windows x64. These cross
checks did not execute the macOS or Windows tests.
