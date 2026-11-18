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
