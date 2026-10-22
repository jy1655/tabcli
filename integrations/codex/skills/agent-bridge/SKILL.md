---
name: agent-bridge
description: Delegate work to installed Claude Code, Codex, Agy, or Pi CLI sessions through Agent Bridge; retrieve exact results, continue conversations, and close task-owned sessions. Use when the user asks to work with another coding assistant or manage a Bridge delegation, not for ordinary work by the current assistant alone.
---

# Agent Bridge

Use the installed `tabcli` engine to delegate the user's work. The invoking assistant and
target provider are separate roles: any supported host can delegate to `codex`, `claude`,
`agy`, or `pi`. Preserve the user's provider, workspace, scope, and permission choices.
If this is itself a delegated task, do not delegate it again unless the task asks for that.

## Before launching

- Resolve `tabcli` with the host shell (`Get-Command tabcli` in PowerShell;
  `command -v tabcli` in POSIX shells) and run `tabcli --version`. This workflow requires
  tabcli **0.4.22 or newer** and the commands shown below in `tabcli --help`.
  Read [setup and recovery](references/setup.md) if the executable, version, platform,
  provider, shell permission, or workspace trust prevents progress.
- Select the actual project directory explicitly with `--workspace`. Use the user's
  requested worktree when supplied. A child shares that directory: agree on file ownership
  or use a separate Git worktree when concurrent edits would collide. Do not switch another
  session's branch or overwrite its work.
- Only the selected target CLI needs to be installed and authenticated. Bridge supports
  macOS and native Windows; Windows needs PowerShell 7 and defaults to `windows-console`.
  On macOS, allow terminal detection or use the user's supported `--terminal` choice.
- Keep the provider's permission and trust controls. Do not add `--yolo`, answer a trust
  dialog, edit consent stores, or relax the host's shell restrictions as setup defaults.
  Forward bypass/model/effort options only when the user actually requests them.
- On a host that ends child processes when a shell command returns, keep its native shell
  handle alive through launch, result, follow-up and close. Native Windows Agy 1.4.5 needs
  this in its console-window fallback; read [the verified host procedure](references/setup.md)
  before its first launch. `--detach` does not preserve a host-owned process tree.

## Launch, read, continue

Write a UTF-8 prompt file outside tracked source unless the task calls for a tracked artifact.
Describe the concrete task, permitted edits, workspace, expected deliverable, and verification.
Use an absolute path, quote shell arguments, and keep the file until the command has read it.
Prompt files avoid shell interpolation; they do not remove a provider's transport limits.

```sh
tabcli ask claude --workspace "/absolute/project" --prompt-file "/absolute/task.txt" --detach --json
```

Replace `claude` with the requested provider; `--terminal windows-console` is optional on Windows.
`--detach` returns after launch/delivery processing; it does not mean the task finished.
If the shell tool itself returns a running process handle, poll that same handle for the
command's JSON. Do not launch another copy because the observation tool yielded.

Retain the returned **`session` and `request_id`**, including on an error that supplies them.
Track which sessions this task created in the conversation or its existing task notes.
Read that exact request rather than `--latest`, which can name a later turn:

```sh
tabcli result SESSION --request REQUEST --wait --timeout-secs 60 --json
```

Inspect `ok`, `request_state`, `result`, and `error`. A detached acceptance or successful
read command is not a successful provider result. Consume the completed answer and inspect
any files/tests it claims to have produced. If more work is needed, continue the same ready
session and retain the **new** request id:

```sh
tabcli tell SESSION --prompt-file "/absolute/follow-up.txt" --detach --json
tabcli result SESSION --request NEW_REQUEST --wait --timeout-secs 60 --json
```

Keep follow-ups concise; use paths to long material that the child can read. In particular,
Windows Codex's queue still passes its message as a command-line argument even with
`--prompt-file`. For multiple tasks, `tabcli wait SESSION/REQUEST ... --timeout-secs 60 --json`
observes exact addresses together. Continue useful independent work between bounded waits.

## When an operation does not finish

A result timeout is only an observation deadline. It neither cancels the request nor proves
that the prompt was not sent. Poll the same request or inspect it:

```sh
tabcli inspect SESSION --json
tabcli inspect SESSION --timeline --request REQUEST --json
tabcli doctor SESSION --json
```

Never automatically resend an uncertain delivery or restart a request merely because a
wait expired. A failed or unresolved request needs its recorded reason assessed; do not
rewrite Bridge records, synthesize a completion, or type directly into the managed terminal.
Use [setup and recovery](references/setup.md) for missing programs, trust/permission gates,
incompatible versions, or unavailable provider capabilities. Report a genuine blocker with
the request address and required user action, preserving the pending work.

## Finish and close

Complete the necessary follow-ups and retrieve their results. Then close each session created
for this task, including after a failure when no further work in that session is needed:

```sh
tabcli close-session SESSION --explicit --json
```

Check `closed` and `ok`; report any cleanup that could not be confirmed. Preserve sessions
that predated this task or belong to another task. If close reports sessions started from
this one, verify they belong to this task, then close those ids as well; use
`--with-started-from` only when all listed children are task-owned. It covers one level,
so inspect its report for further descendants or incomplete scans. Keep sessions open only
when the user requests continuing them, and hand off their exact addresses.

Summarize the result, verification, and remaining blockers. A skill guides cleanup while
the host is running; it cannot guarantee cleanup after the host crashes or exits.
