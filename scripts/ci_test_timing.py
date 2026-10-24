#!/usr/bin/env python3
"""Record the serial Cargo test run; compare artifacts without a wall-clock gate."""

import argparse
from datetime import datetime, timezone
import html
import json
import math
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import time


COMMAND = ["cargo", "test", "--all-targets", "--all-features", "--", "--test-threads=1"]
ENVIRONMENT = (
    "RUNNER_OS", "RUNNER_ARCH", "RUNNER_ENVIRONMENT", "ImageOS", "ImageVersion",
    "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_JOB", "GITHUB_EVENT_NAME",
    "GITHUB_SHA", "GITHUB_REF", "GITHUB_WORKFLOW", "RUNNER_NAME", "RUSTUP_TOOLCHAIN",
    "NUMBER_OF_PROCESSORS", "PROCESSOR_IDENTIFIER",
)
ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
RESULT = re.compile(
    r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;.*"
    r" finished in ([0-9.]+)s$"
)
TEST = re.compile(r"^test (\S+)(?: - should panic)? \.\.\. (ok|FAILED|ignored(?:,.*)?)$")
FINISHED = re.compile(r"^\s*Finished (.+) in ((?:(\d+)m )?([0-9.]+)s)$")
MAX_LINE = 1024 * 1024


class Observation:
    """Output arrival intervals include buffering and harness/fixture work."""

    def __init__(self):
        self.buffer = b""
        self.discard_line = False
        self.unparsed_long_lines = 0
        self.non_utf8_lines = 0
        self.suites = []
        self.tests = []
        self.compilation = []
        self.suite = None
        self.last_test = None
        self.target = None
        self.line_started = None

    def line(self, raw, elapsed, started):
        try:
            line = ANSI.sub(b"", raw).decode("utf-8").rstrip("\r\n")
        except UnicodeDecodeError:
            self.non_utf8_lines += 1
            return
        running = re.match(r"^\s*Running (.+) \([^\r\n]+\)$", line)
        if running:
            self.target = running[1].replace("\\", "/")
        finished = FINISHED.match(line)
        if finished:
            self.compilation.append({
                "profile": finished[1],
                "reported_seconds": 60 * int(finished[3] or 0) + float(finished[4]),
                "observed_at_seconds": elapsed,
            })
        if re.match(r"^running \d+ tests?$", line):
            self.suite = len(self.suites)
            self.suites.append({"ordinal": self.suite, "target": self.target,
                                "reported_seconds": None, "parsed_tests": 0})
            self.last_test = elapsed
        test = TEST.match(line)
        if test and self.suite is not None:
            self.tests.append({
                "suite": self.suite, "name": test[1], "outcome": test[2],
                "output_interval_seconds": max(0.0, elapsed - self.last_test),
                "observed_start_to_result_seconds": max(0.0, elapsed - started),
            })
            self.suites[self.suite]["parsed_tests"] += 1
            self.last_test = elapsed
        result = RESULT.match(line)
        if result and self.suite is not None:
            self.suites[self.suite].update({
                "outcome": result[1], "passed": int(result[2]), "failed": int(result[3]),
                "ignored": int(result[4]), "reported_seconds": float(result[5]),
            })
            self.suite = None
            self.last_test = None

    def feed(self, data, elapsed, final=False):
        # Only the parser is bounded. The raw log and live output retain every byte.
        for part in data.splitlines(keepends=True):
            newline = part.endswith(b"\n")
            if not self.discard_line:
                if not self.buffer:
                    self.line_started = elapsed
                self.buffer += part
                if len(self.buffer) > MAX_LINE:
                    self.buffer = b""
                    self.discard_line = True
                    self.unparsed_long_lines += 1
            if newline:
                if not self.discard_line:
                    self.line(self.buffer, elapsed, self.line_started)
                self.buffer = b""
                self.discard_line = False
        if final and self.buffer and not self.discard_line:
            self.line(self.buffer, elapsed, self.line_started)
            self.buffer = b""

    def document(self):
        return {"suites": self.suites, "completed_tests": self.tests,
                "cargo_finished": self.compilation,
                "unparsed_long_lines": self.unparsed_long_lines,
                "non_utf8_lines": self.non_utf8_lines}


def probe(command, cwd, errors):
    try:
        result = subprocess.run(command, cwd=cwd, capture_output=True, text=True,
                                encoding="utf-8", errors="replace", timeout=10)
        if result.returncode == 0:
            return result.stdout.strip()
        errors.append(f"{' '.join(command)}: exit {result.returncode}: {result.stderr.strip()}")
    except (OSError, subprocess.TimeoutExpired) as error:
        errors.append(f"{' '.join(command)}: {error}")
    return None


def metadata(cwd):
    errors = []
    dirty = probe(["git", "status", "--porcelain"], cwd, errors)
    return {
        # GITHUB_SHA can identify the workflow rather than the release checkout.
        "source_sha": probe(["git", "rev-parse", "HEAD"], cwd, errors),
        "source_tree": probe(["git", "rev-parse", "HEAD^{tree}"], cwd, errors),
        "source_dirty": bool(dirty) if dirty is not None else None,
        "rustc": probe(["rustc", "--version", "--verbose"], cwd, errors),
        "cargo": probe(["cargo", "--version"], cwd, errors),
        "python": platform.python_version(),
        "cpu_count": os.cpu_count(),
        "platform": {"system": platform.system(), "machine": platform.machine(),
                     "release": platform.release()},
        "environment": {key: os.environ.get(key) for key in ENVIRONMENT},
        "probe_errors": errors,
    }


def suite_complete(suite):
    return (suite.get("target") is not None and suite.get("reported_seconds") is not None
            and suite.get("parsed_tests") == sum(suite.get(key, 0) for key in ["passed", "failed", "ignored"]))


def save_record(path, data):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(data, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def shell_exit_code(code, windows=None):
    windows = os.name == "nt" if windows is None else windows
    if windows:
        return code - 2**32 if code >= 2**31 else code
    return 128 - code if code < 0 else code


def cell(value):
    return html.escape(str(value)).replace("|", "&#124;").replace("\n", "<br>")


def markdown(data):
    meta, observation = data["metadata"], data["observation"]
    env = meta["environment"]
    lines = ["## Serial test timing", "",
             f"Source: {cell(meta['source_sha'])}", "",
             f"Runner: {cell(env.get('RUNNER_OS'))} / {cell(env.get('RUNNER_ARCH'))}; "
             f"image: {cell(env.get('ImageOS'))} {cell(env.get('ImageVersion'))}", "",
             f"Cargo exit: {data['returncode']}; observed command: {data['elapsed_seconds']:.3f}s.",
             "", "| Suite | Passed | Failed | Ignored | libtest seconds |",
             "| --- | ---: | ---: | ---: | ---: |"]
    for suite in observation["suites"]:
        lines.append("| " + " | ".join(cell(suite.get(key, "unknown")) for key in
                                      ["target", "passed", "failed", "ignored", "reported_seconds"]) + " |")
    lines += ["", "Longest completed-test output intervals (not profiler timings; include "
              "buffering, fixtures and harness overhead):", "",
              "| Test | Outcome | Output interval seconds |", "| --- | --- | ---: |"]
    for test in sorted(observation["completed_tests"],
                       key=lambda item: item["output_interval_seconds"], reverse=True)[:20]:
        lines.append(f"| {cell(test['name'])} | {cell(test['outcome'])} | "
                     f"{test['output_interval_seconds']:.3f} |")
    lines += ["", "Full command, toolchain, Cargo compile summaries and parser diagnostics are "
              "in timings.json. output.log retains combined stdout/stderr. "
              "No performance threshold is applied.", ""]
    return "\n".join(lines)


def run(output, cwd, command=None, stream=None, details=None):
    command = list(COMMAND if command is None else command)
    stream = sys.stdout.buffer if stream is None else stream
    details = metadata(cwd) if details is None else details
    output = Path(output)
    output.mkdir(parents=True, exist_ok=False)
    observation = Observation()
    errors = []
    returncode = None
    started = datetime.now(timezone.utc).isoformat()
    result = {"schema": 1, "status": "running", "started_at": started, "metadata": details,
              "command": command, "returncode": None, "elapsed_seconds": 0.0,
              "errors": errors, "observation": observation.document()}
    # A cancelled runner may never execute finalization. This initial record supplies
    # no completion; output.log is flushed as bytes arrive.
    save_record(output / "timings.json", result)
    begin = time.monotonic()
    with (output / "output.log").open("wb") as log:
        try:
            child = subprocess.Popen(command, cwd=cwd, stdout=subprocess.PIPE,
                                     stderr=subprocess.STDOUT)
        except OSError as error:
            errors.append(str(error))
        else:
            sinks = {"log": log, "console": stream}
            try:
                while data := child.stdout.read1(65536):
                    # A failed sink must not leave Cargo blocked on a full pipe.
                    for name, sink in list(sinks.items()):
                        try:
                            sink.write(data)
                            sink.flush()
                        except OSError as error:
                            errors.append(f"{name}: {error}")
                            del sinks[name]
                    observation.feed(data, time.monotonic() - begin)
                returncode = child.wait()
            except BaseException:
                child.kill()
                child.wait()
                raise
            finally:
                child.stdout.close()
    elapsed = time.monotonic() - begin
    observation.feed(b"", elapsed, final=True)
    result.update(status="completed" if returncode is not None else "spawn_failed",
                  returncode=returncode, elapsed_seconds=elapsed, observation=observation.document())
    summary = markdown(result)
    try:
        save_record(output / "timings.json", result)
        (output / "summary.md").write_text(summary, encoding="utf-8")
    except OSError as error:
        errors.append(f"final artifacts: {error}")
        print(f"Could not finish timing artifacts: {error}", file=sys.stderr)
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        try:
            with open(path, "a", encoding="utf-8") as handle:
                handle.write(summary)
        except OSError as error:
            print(f"Could not append timing summary: {error}", file=sys.stderr)
    if returncode is None:
        print("Could not run serial tests: " + "; ".join(errors), file=sys.stderr)
        return 1
    if errors and returncode == 0:
        print("Test timing capture failed: " + "; ".join(errors), file=sys.stderr)
        return 1
    return shell_exit_code(returncode)


def read_record(path):
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if (not isinstance(data, dict) or type(data.get("schema")) is not int
            or data["schema"] != 1 or not isinstance(data.get("metadata"), dict)):
        raise ValueError(f"{path}: unsupported timing record")
    if (not isinstance(data.get("command"), list)
            or not all(isinstance(x, str) for x in data["command"])
            or not isinstance(data.get("errors"), list)
            or data.get("returncode") is not None and type(data["returncode"]) is not int):
        raise ValueError(f"{path}: invalid command outcome")
    def seconds(value):
        return type(value) in (float, int) and math.isfinite(value) and value >= 0
    if not seconds(data.get("elapsed_seconds")):
        raise ValueError(f"{path}: invalid elapsed time")
    meta = data["metadata"]
    if not isinstance(meta.get("environment"), dict) or not isinstance(meta.get("platform"), dict):
        raise ValueError(f"{path}: missing environment metadata")
    for key in ["source_sha", "rustc", "cargo"]:
        if key not in meta or meta[key] is not None and not isinstance(meta[key], str):
            raise ValueError(f"{path}: invalid {key}")
    obs = data.get("observation")
    if not isinstance(obs, dict) or not isinstance(obs.get("suites"), list):
        raise ValueError(f"{path}: missing suite observations")
    for key in ["non_utf8_lines", "unparsed_long_lines"]:
        if type(obs.get(key)) is not int or obs[key] < 0:
            raise ValueError(f"{path}: invalid parser diagnostics")
    for suite in obs["suites"]:
        if (not isinstance(suite, dict) or "reported_seconds" not in suite
                or suite["reported_seconds"] is not None and not seconds(suite["reported_seconds"])):
            raise ValueError(f"{path}: invalid suite time")
        if suite["reported_seconds"] is not None and any(
            type(suite.get(key)) is not int or suite[key] < 0 for key in ["passed", "failed", "ignored"]
        ):
            raise ValueError(f"{path}: invalid suite counts")
        if (suite.get("target") is not None and not isinstance(suite["target"], str)
                or type(suite.get("parsed_tests")) is not int or suite["parsed_tests"] < 0):
            raise ValueError(f"{path}: invalid suite identity or parsed count")
    return data


def compare(before, after):
    mismatches = []
    for key in ["rustc", "cargo", "platform", "cpu_count"]:
        left, right = before["metadata"].get(key), after["metadata"].get(key)
        if left is None or right is None or left != right:
            mismatches.append(key)
    for key in ["RUNNER_OS", "RUNNER_ARCH", "RUNNER_ENVIRONMENT", "ImageOS", "ImageVersion",
                "PROCESSOR_IDENTIFIER", "NUMBER_OF_PROCESSORS"]:
        left = before["metadata"]["environment"].get(key)
        right = after["metadata"]["environment"].get(key)
        if left is None or right is None or left != right:
            mismatches.append(key)
    if before["command"] != after["command"]:
        mismatches.append("command")
    if any(data["metadata"].get("source_dirty") is not False for data in [before, after]):
        mismatches.append("dirty or unknown source checkout")
    def suite_counts(data):
        return sorted((str(s.get("target")), *(s.get(key, 0) for key in ["passed", "failed", "ignored"]))
                      for s in data["observation"]["suites"])
    if suite_counts(before) != suite_counts(after):
        mismatches.append("suite counts")
    if before["returncode"] != 0 or after["returncode"] != 0:
        mismatches.append("unsuccessful command")
    for label, data in [("before", before), ("after", after)]:
        obs = data["observation"]
        if (data.get("status") != "completed" or data["errors"] or not obs["suites"]
                or obs["unparsed_long_lines"] or obs["non_utf8_lines"]
                or any(not suite_complete(s) for s in obs["suites"])
                or len({s["target"] for s in obs["suites"]}) != len(obs["suites"])):
            mismatches.append(f"{label} incomplete observation")
    lines = ["Serial test timing comparison (observations, not a performance gate)",
             f"Source: {before['metadata']['source_sha']} -> {after['metadata']['source_sha']}",
             f"Trees: {before['metadata'].get('source_tree')} -> {after['metadata'].get('source_tree')}"]
    if mismatches:
        lines += ["Conditions differ or are unknown: " + ", ".join(mismatches),
                  "Do not attribute the difference to a code change."]
    else:
        lines += ["Recorded conditions match; hosted load and caches remain uncontrolled."]
    values = []
    if before.get("status") == after.get("status") == "completed":
        values.append(("Command including compilation", before["elapsed_seconds"], after["elapsed_seconds"]))
    else:
        lines.append("Unfinished run; no command duration delta is computed.")
    def unique_suites(data):
        suites = data["observation"]["suites"]
        return {s["target"]: s for s in suites
                if s["target"] is not None and sum(t["target"] == s["target"] for t in suites) == 1}
    left_suites, right_suites = unique_suites(before), unique_suites(after)
    if left_suites.keys() != right_suites.keys():
        lines.append("Suite sets differ; no libtest total delta is computed.")
    elif (left_suites and len(left_suites) == len(before["observation"]["suites"])
          and len(right_suites) == len(after["observation"]["suites"])
          and all(suite_complete(s) for s in [*left_suites.values(), *right_suites.values()])):
        values.append(("Reported libtest total", sum(s["reported_seconds"] for s in left_suites.values()),
                       sum(s["reported_seconds"] for s in right_suites.values())))
    for target in sorted(left_suites.keys() & right_suites.keys(), key=str):
        left, right = left_suites[target]["reported_seconds"], right_suites[target]["reported_seconds"]
        if left is not None and right is not None:
            values.append((str(target), left, right))
    for name, left, right in values:
        lines.append(f"{name}: {left:.3f}s -> {right:.3f}s ({right - left:+.3f}s)")
    for label, data in [("Before", before), ("After", after)]:
        totals = {key: sum(s.get(key, 0) for s in data["observation"]["suites"])
                  for key in ["passed", "failed", "ignored"]}
        lines.append(f"{label} suite counts: {totals}")
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    record = commands.add_parser("run", help="run the unchanged serial Cargo test command")
    record.add_argument("--output-dir", type=Path, required=True, help="new artifact directory")
    comparison = commands.add_parser("compare", help="compare two downloaded timings.json files")
    comparison.add_argument("before", type=Path)
    comparison.add_argument("after", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.action == "run":
            return run(args.output_dir, Path.cwd())
        print(compare(read_record(args.before), read_record(args.after)), end="")
        return 0
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Test timing error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
