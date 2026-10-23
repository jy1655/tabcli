#!/usr/bin/env python3
"""Check macOS Claude ask/tell completion against a configured blocking Stop hook."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time


PROMPT = (
    "This is an isolated completion regression probe. Do not use tools, edit files, "
    "or spawn agents. Respond with exactly PROBE-DRAFT. If this request contains a "
    "delegation completion marker, retain it on the final line. Do not invent a "
    "marker if none was supplied. Follow a configured Stop hook correction while "
    "preserving any supplied marker."
)

HOOK = '''import json, pathlib, sys, time
payload = json.load(sys.stdin)
directory = pathlib.Path(sys.argv[1])
with (directory / "stops.jsonl").open("a") as stream:
    stream.write(json.dumps({"time": time.time(), "payload": payload}) + "\\n")
if "PROBE-DRAFT" in payload.get("last_assistant_message", "") and not payload.get("stop_hook_active", False):
    print(json.dumps({"decision": "block", "reason": "Replace PROBE-DRAFT with PROBE-FINAL. Retain any supplied delegation completion marker on its own final line. Do not call tools or delegate. This is the one required continuation."}))
'''

OBSERVER = '''import json, pathlib, sys, time
payload = json.load(sys.stdin)
directory = pathlib.Path(sys.argv[1])
with (directory / "events.jsonl").open("a") as stream:
    stream.write(json.dumps({"time": time.time(), "payload": payload}) + "\\n")
'''


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def run_json(command, output, name, environment=None, timeout=110):
    try:
        result = subprocess.run(
            command, env=environment, text=True, capture_output=True, timeout=timeout
        )
    except subprocess.TimeoutExpired as error:
        for suffix, value in ((".json", error.stdout), (".stderr", error.stderr)):
            if isinstance(value, bytes):
                value = value.decode("utf-8", errors="replace")
            (output / (name + suffix)).write_text(value or "", encoding="utf-8")
        raise
    (output / (name + ".json")).write_text(result.stdout, encoding="utf-8")
    (output / (name + ".stderr")).write_text(result.stderr, encoding="utf-8")
    value = json.loads(result.stdout)
    return value, result.returncode


def records(path):
    if not path.exists():
        return []
    return [json.loads(line)["payload"] for line in path.read_text().splitlines()]


def owned_sessions(state):
    return sorted(path.name for path in state.iterdir()
                  if re.fullmatch(r"session-[A-Za-z0-9]+", path.name)
                  and not path.is_symlink() and path.is_dir())


def capture_surface(output, terminal_session):
    if not terminal_session:
        return
    script = '''on run argv
tell application "iTerm2"
 repeat with w in windows
  repeat with t in tabs of w
   repeat with s in sessions of t
    if unique id of s is item 1 of argv then return contents of s
   end repeat
  end repeat
 end repeat
end tell
error "The owned iTerm2 session was not found"
end run
'''
    result = subprocess.run(
        ["osascript", "-", terminal_session], input=script,
        text=True, capture_output=True, timeout=15,
    )
    (output / "surface.txt").write_text(result.stdout, encoding="utf-8")
    (output / "surface.stderr").write_text(result.stderr, encoding="utf-8")
    if result.returncode:
        raise RuntimeError("Could not inspect the owned iTerm2 surface")


def prepare_probe(output, real_claude):
    shim = output / "bin"
    shim.mkdir()
    plugin = output / "observer-plugin"
    (plugin / ".claude-plugin").mkdir(parents=True)
    (plugin / "hooks").mkdir()
    write_json(plugin / ".claude-plugin/plugin.json", {
        "name": "tabcli-stop-completion-probe", "version": "1.0.0",
        "description": "Observe the isolated Stop completion regression probe.",
    })
    write_json(plugin / "hooks/hooks.json", {"modules": ["./register.js"]})
    (output / "stop.py").write_text(HOOK, encoding="utf-8")
    (output / "observe.py").write_text(OBSERVER, encoding="utf-8")
    observer_command = json.dumps([sys.executable, str(output / "observe.py")])
    (plugin / "hooks/register.js").write_text(
        "async function observe($, event, data) {\n"
        ' const directory = await $.env.get("TABCLI_STOP_PROBE_DIRECTORY");\n'
        " if (!directory) return;\n"
        " const conversation = await $.session.id();\n"
        f" await $.process.run([...{observer_command}, directory], "
        "{stdin: JSON.stringify({event, conversation, data}), timeoutMs: 10000});\n"
        "}\nexport function register(on) {\n"
        ' on("session.start", async ($, e, next) => {\n'
        '  if (e.agentId === undefined) await observe($, "session.start", {});\n'
        "  return next(e);\n });\n"
        ' on("turn.start", async ($, e, next) => {\n'
        '  if (e.agentId === undefined) await observe($, "turn.start", '
        "{turnId: e.turnId, text: e.text});\n  return next(e);\n });\n"
        ' on("turn.complete", async ($, e, next) => {\n'
        '  if (e.agentId === undefined) await observe($, "turn.complete", '
        "{turnId: e.turnId, reason: e.reason, isAborted: e.isAborted, answer: e.answer});\n"
        "  return next(e);\n });\n}\n", encoding="utf-8",
    )
    wrapper = f"#!{sys.executable}\n" + '''import json, os, pathlib, shlex, sys
args = sys.argv[1:]
root = pathlib.Path(ROOT)
if "--name" in args and "--settings" in args:
    name = args[args.index("--name") + 1]
    directory = root / name
    directory.mkdir()
    index = args.index("--settings") + 1
    settings = json.loads(pathlib.Path(args[index]).read_text())
    command = shlex.join([sys.executable, str(root / "stop.py"), str(directory)])
    settings.setdefault("hooks", {}).setdefault("Stop", []).append({"hooks": [{"type": "command", "command": command, "timeout": 15}]})
    merged = directory / "probe-settings.json"
    merged.write_text(json.dumps(settings))
    args[index] = str(merged)
    args = ["--plugin-dir", str(root / "observer-plugin")] + args
    os.environ["TABCLI_STOP_PROBE_DIRECTORY"] = str(directory)
os.execv(REAL_CLAUDE, [REAL_CLAUDE] + args)
'''
    wrapper = wrapper.replace("ROOT", repr(str(output))).replace(
        "REAL_CLAUDE", repr(str(real_claude))
    )
    (shim / "claude").write_text(wrapper, encoding="utf-8")
    (shim / "claude").chmod(0o700)
    (output / "prompt.txt").write_text(PROMPT, encoding="utf-8")
    environment = os.environ.copy()
    environment["PATH"] = str(shim) + os.pathsep + environment.get("PATH", "")
    state = output / "state"
    state.mkdir(mode=0o700)
    environment["AGENT_BRIDGE_NATIVE_STATE_DIR"] = str(state)
    return environment


def verify_turn(bridge, output, session, request, ordinal, expected, timeout, environment):
    directory = output / session
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        completed = [p for p in records(directory / "events.jsonl")
                     if p.get("event") == "turn.complete"]
        if len(completed) >= ordinal:
            break
        time.sleep(0.2)
    else:
        raise RuntimeError("The observer did not record the expected turn.complete")
    stops = records(directory / "stops.jsonl")
    current = stops[(ordinal - 1) * 2:ordinal * 2]
    if len(current) != 2 or current[0].get("stop_hook_active") is not False:
        raise RuntimeError("The configured first Stop was not observed")
    if current[1].get("stop_hook_active") is not True:
        raise RuntimeError("The blocked Stop did not continue")
    if not current[0].get("last_assistant_message", "").startswith("PROBE-DRAFT"):
        raise RuntimeError("The first Stop did not contain the probe draft")
    if not current[1].get("last_assistant_message", "").startswith("PROBE-FINAL"):
        raise RuntimeError("The continued Stop did not contain the probe final")
    official = completed[ordinal - 1]
    starts = [p for p in records(directory / "events.jsonl")
              if p.get("event") == "turn.start"]
    if len(starts) != ordinal or len(completed) != ordinal:
        raise RuntimeError("Unexpected number of official main-loop turns")
    started = starts[ordinal - 1]
    if (not official.get("data", {}).get("turnId")
            or started.get("conversation") != official.get("conversation")
            or started.get("data", {}).get("turnId") != official["data"]["turnId"]):
        raise RuntimeError("Official turn start and completion identities differ")
    if official["data"].get("reason") != "answer" or official["data"].get("isAborted") is not False:
        raise RuntimeError("Official turn did not finish normally")
    if not official.get("data", {}).get("answer", "").startswith("PROBE-FINAL"):
        raise RuntimeError("Official completion did not contain the probe final")
    result, code = run_json(
        [str(bridge), "result", session, "--request", request,
         "--wait", "--timeout-secs", "30", "--json"],
        output, f"result-{ordinal}", environment, timeout=40,
    )
    if code or result.get("request_state") != "completed":
        raise RuntimeError("The exact Bridge request did not complete")
    if result.get("provider_session_id") != official.get("conversation"):
        raise RuntimeError("The provider conversation does not match")
    actual = result.get("result")
    print(f"{'ask' if ordinal == 1 else 'tell'}: provider=PROBE-FINAL bridge={actual}", flush=True)
    if actual != expected:
        raise RuntimeError(f"Expected Bridge {expected}, got {actual!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bridge", type=Path, required=True)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expect", choices=["draft", "final"], default="final")
    parser.add_argument("--yolo", action="store_true")
    parser.add_argument("--timeout-secs", type=int, default=120)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("This probe requires macOS and iTerm2")
    real_claude = shutil.which("claude")
    if not real_claude:
        parser.error("claude must be installed and signed in")
    bridge = args.bridge.resolve(strict=True)
    workspace = args.workspace.resolve(strict=True)
    output = args.output.absolute()
    output.mkdir(parents=True, exist_ok=False)
    output = output.resolve()
    environment = prepare_probe(output, Path(real_claude).resolve())
    state = Path(environment["AGENT_BRIDGE_NATIVE_STATE_DIR"])
    session = None
    terminal_session = None
    failed = False
    try:
        command = [str(bridge), "ask", "claude", "--workspace", str(workspace),
                   "--terminal", "iterm2", "--title", "Claude Stop completion probe",
                   "--prompt-file", str(output / "prompt.txt"), "--detach",
                   "--timeout-secs", str(args.timeout_secs), "--json"]
        if args.yolo:
            command.append("--yolo")
        launch, code = run_json(command, output, "launch", environment,
                                args.timeout_secs + 20)
        session = launch.get("session")
        terminal_session = launch.get("iterm_session_id")
        if code or not session or not launch.get("ok"):
            raise RuntimeError("The managed probe launch failed; inspect launch.json")
        expected = "PROBE-" + args.expect.upper()
        verify_turn(bridge, output, session, launch["request_id"], 1,
                    expected, args.timeout_secs, environment)
        followup, code = run_json(
            [str(bridge), "tell", session, "--prompt-file", str(output / "prompt.txt"),
             "--detach", "--timeout-secs", str(args.timeout_secs), "--json"],
            output, "tell", environment, timeout=args.timeout_secs + 20,
        )
        if code or not followup.get("ok"):
            raise RuntimeError("The managed follow-up failed; inspect tell.json")
        verify_turn(bridge, output, session, followup["request_id"], 2,
                    expected, args.timeout_secs, environment)
        history, code = run_json(
            [str(bridge), "result", session, "--list", "--json"], output, "history", environment,
        )
        published = [event.get("request_id") for event in history.get("events", [])]
        if code or sorted(published) != sorted([launch["request_id"], followup["request_id"]]):
            raise RuntimeError("Expected exactly one published event per probe request")
    except (Exception, KeyboardInterrupt) as error:
        failed = True
        print(f"FAIL: {error}", file=sys.stderr)
    finally:
        sessions = owned_sessions(state)
        write_json(output / "cleanup-sessions.json", sessions)
        if session and session not in sessions:
            failed = True
            print(f"FAIL: owned state is missing for {session}; cleanup unconfirmed", file=sys.stderr)
        for owned in sessions:
            cleanup_output = output if len(sessions) == 1 else output / owned / "cleanup"
            cleanup_output.mkdir(parents=True, exist_ok=True)
            try:
                run_json([str(bridge), "inspect", owned, "--json"],
                         cleanup_output, "inspect", environment)
                surface = terminal_session if owned == session else None
                handle = state / owned / "terminal.json"
                if surface is None and handle.exists():
                    record = json.loads(handle.read_text())
                    if record.get("terminal") == "iterm2":
                        surface = record.get("session_id", record.get("iterm_session_id"))
                capture_surface(cleanup_output, surface)
            except Exception as error:
                failed = True
                print(f"FAIL: inspection for {owned}: {error}", file=sys.stderr)
            finally:
                try:
                    closed, code = run_json(
                        [str(bridge), "close-session", owned, "--explicit",
                         "--timeout-secs", "20", "--json"],
                        cleanup_output, "close", environment, timeout=35,
                    )
                    if code or not closed.get("ok") or not closed.get("closed"):
                        raise RuntimeError("close-session did not confirm closure")
                except Exception as error:
                    failed = True
                    print(f"FAIL: cleanup unconfirmed for {owned}: {error}", file=sys.stderr)
    if not failed:
        print(f"PASS: ask and tell match expected {args.expect}; owned session closed")
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
