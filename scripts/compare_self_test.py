#!/usr/bin/env python3
"""Compare two saved self-test experiments offline, using only the standard library."""

import argparse
from collections import Counter
import hashlib
import json
import math
import os
from pathlib import Path
import re
import stat
import statistics
import sys


MAX_BYTES = 8 * 1024 * 1024
MAX_RUNS = 1000
MAX_STEPS = 64
OUTCOMES = ("passed", "failed", "timed_out", "unsupported", "not_verified")
STEPS = ("admission", "ask", "initial_result", "tell", "follow_up_result", "cleanup")
SKIP_REASON = "not run because a preceding step was not verified"
U128_MAX = 2 ** 128 - 1
# Parser-only invalid value: output retains its source pointer, never this object.
UNPARSED_INTEGER = object()
CONDITIONS = (
    "provider", "provider_version", "provider_backend", "terminal", "terminal_version",
    "os", "os_version", "arch", "machine", "model", "effort", "permission_mode",
    "start_state", "workspace_scenario", "configuration", "scenario",
)
BRIDGE_FIELDS = ("version", "binary_sha256", "source_revision", "source_dirty")
UNKNOWN = {"unknown", "auto", "unspecified", "not_verified", ""}


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON number: {value}")


def bounded_integer(token):
    # Bound conversion itself, including on Python versions without a digit limit.
    return int(token) if len(token.lstrip("-")) <= 39 else UNPARSED_INTEGER


def read_experiment(path):
    # O_NONBLOCK prevents a named pipe from blocking before the regular-file check.
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NONBLOCK", 0))
    with os.fdopen(descriptor, "rb") as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size > MAX_BYTES:
            raise ValueError("input must be a regular file of at most 8 MiB")
        raw = stream.read(MAX_BYTES + 1)
        after = os.fstat(stream.fileno())
    if (len(raw) > MAX_BYTES or len(raw) != before.st_size
            or (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns)):
        raise ValueError("input changed while reading or exceeded 8 MiB")
    document = json.loads(raw.decode("utf-8"), object_pairs_hook=unique_object,
                          parse_constant=reject_constant, parse_int=bounded_integer)
    if not isinstance(document, dict):
        raise ValueError("experiment must be a JSON object")
    layout = "experiment"
    if "runs" not in document:
        # Older reports remain observable without inventing their missing controls.
        document = {"runs": [{"report": document}]}
        layout = "bare_report"
    runs = document.get("runs")
    if not isinstance(runs, list) or len(runs) > MAX_RUNS:
        raise ValueError(f"runs must be an array of at most {MAX_RUNS} samples")
    for run in runs:
        report = run.get("report") if isinstance(run, dict) else None
        if isinstance(report, dict):
            for key in ("steps", "cleanup_sessions"):
                if isinstance(report.get(key), list) and len(report[key]) > MAX_STEPS:
                    raise ValueError(f"a report may contain at most {MAX_STEPS} {key}")
    return document, {"path": str(Path(path).absolute()), "layout": layout, "size_bytes": len(raw),
                      "sha256": hashlib.sha256(raw).hexdigest()}


def known_text(value):
    return (isinstance(value, str) and len(value) <= 1024
            and value.strip().lower() not in UNKNOWN
            and not any(ord(character) < 32 for character in value))


def identity(document, label, insufficient):
    if type(document.get("schema_version")) is not int or document["schema_version"] != 1:
        insufficient.append(f"{label}: experiment schema_version must be 1")
    conditions = document.get("conditions")
    if not isinstance(conditions, dict):
        conditions = {}
    for field in CONDITIONS:
        if not known_text(conditions.get(field)):
            insufficient.append(f"{label}: missing/unknown condition {field}")
    if type(conditions.get("isolated")) is not bool:
        insufficient.append(f"{label}: condition isolated must be a boolean")
    # Additional controls must also agree; none are silently ignored.
    for key, value in conditions.items():
        if key != "isolated" and not known_text(value):
            insufficient.append(f"{label}: invalid condition {key!r}")
    bridge = document.get("bridge")
    if not isinstance(bridge, dict):
        bridge = {}
    for field in ("version", "binary_sha256", "source_revision"):
        if not known_text(bridge.get(field)):
            insufficient.append(f"{label}: missing Bridge identity {field}")
    if not isinstance(bridge.get("binary_sha256"), str) or not re.fullmatch(
            r"[0-9a-f]{64}", bridge["binary_sha256"]):
        insufficient.append(f"{label}: binary_sha256 must be lowercase SHA-256")
    if type(bridge.get("source_dirty")) is not bool:
        insufficient.append(f"{label}: Bridge source_dirty must be a boolean")
    return conditions, bridge


def outcome(value):
    return value if isinstance(value, str) and value in OUTCOMES else "invalid_or_missing"


def elapsed(record):
    if "elapsed_ms" not in record or record["elapsed_ms"] is None:
        return {"state": "missing"}
    value = record["elapsed_ms"]
    if type(value) is not int or not 0 <= value <= U128_MAX:
        return {"state": "invalid", "reason": "expected integer milliseconds in u128 range"}
    return {"state": "valid", "value": value}


def skipped_steps(report, steps):
    # Only the schema-1 orchestrator's complete source order gives gate authority.
    if (type(report.get("schema_version")) is not int or report["schema_version"] != 1
            or [step.get("name") if isinstance(step, dict) else None for step in steps]
            != list(STEPS[1:])):
        return set()
    return {index for index in range(1, 4)
            if outcome(steps[index - 1].get("outcome")) in OUTCOMES[1:]
            and steps[index].get("outcome") == "not_verified"
            and type(steps[index].get("elapsed_ms")) is int
            and steps[index]["elapsed_ms"] == 0
            and steps[index].get("reason") == SKIP_REASON
            and "request_address" in steps[index] and steps[index]["request_address"] is None
            and "event_address" in steps[index] and steps[index]["event_address"] is None}


def distribution(samples):
    values = sorted(sample["value"] for sample in samples if sample["state"] == "valid")
    count = len(values)
    return {
        "n": count, "missing": sum(s["state"] == "missing" for s in samples),
        "invalid": sum(s["state"] == "invalid" for s in samples),
        "not_run": sum(s["state"] == "not_run" for s in samples),
        "min_ms": min(values) if values else None,
        "median_ms": statistics.median(values) if values else None,
        "mean_ms": statistics.fmean(values) if values else None,
        "max_ms": max(values) if values else None,
        "p95_ms": values[math.ceil(.95 * count) - 1] if count >= 20 else None,
        "sample_limit": "no_valid_samples" if not count else (
            "single_sample" if count == 1 else "small_sample" if count < 5 else "descriptive_only"),
        "p95_limit": "nearest_rank" if count >= 20 else "requires_20_valid_samples",
    }


def metric(samples):
    return {"all": distribution(samples), "by_outcome": {
        key: distribution([sample for sample in samples if sample["outcome"] == key])
        for key in (*OUTCOMES, "invalid_or_missing")
    }, "samples": samples}


def summarize(document, provenance, label, insufficient, incompatible):
    conditions, bridge = identity(document, label, insufficient)
    runs = document["runs"]
    if not runs:
        insufficient.append(f"{label}: no samples")
    counts = Counter({key: 0 for key in (*OUTCOMES, "invalid_or_missing")})
    exit_codes = Counter()
    cleanup_counts = Counter({key: 0 for key in (*OUTCOMES, "invalid_or_missing")})
    cleanup_run_failures = 0
    cleanup_steps_missing = 0
    sample_records = []
    markers = set()
    step_names = set(STEPS)
    for index, run in enumerate(runs):
        pointer = f"/runs/{index}"
        here = f"{label}{pointer}"
        if not isinstance(run, dict):
            run = {}
        code = run.get("exit_code")
        exit_codes[str(code) if type(code) is int else "missing_or_invalid"] += 1
        if type(code) is not int:
            insufficient.append(f"{here}: missing/invalid exit_code")
        report = run.get("report")
        if not isinstance(report, dict):
            report = {}
            insufficient.append(f"{here}: missing self-test report; sample retained")
        result = outcome(report.get("outcome"))
        counts[result] += 1
        if result == "invalid_or_missing":
            insufficient.append(f"{here}: missing/unknown original outcome")
        if type(report.get("schema_version")) is not int or report["schema_version"] != 1:
            insufficient.append(f"{here}: unsupported/missing self-test schema_version")
        for field in ("provider", "provider_version", "terminal", "isolated"):
            observed = report.get(field)
            valid = (type(observed) is bool if field == "isolated" else
                     observed is None and field in ("provider_version", "terminal")
                     or known_text(observed))
            if field not in report or not valid:
                insufficient.append(f"{here}: missing/invalid report identity {field}")
            # Early failures can have no discovered version or terminal. The wrapper's
            # explicit collection metadata is required even for these samples.
            if (valid and field in conditions and observed is not None
                    and observed != conditions[field]):
                incompatible.append(f"{here}: report disagrees with condition {field}")
        if not known_text(report.get("bridge_version")):
            insufficient.append(f"{here}: missing/invalid report Bridge version")
        elif "version" in bridge and report["bridge_version"] != bridge["version"]:
            incompatible.append(f"{here}: report disagrees with Bridge version")
        marker = report.get("marker")
        if not known_text(marker):
            insufficient.append(f"{here}: missing self-test marker identity")
        elif marker in markers:
            insufficient.append(f"{here}: repeated self-test marker; samples are not distinct")
        else:
            markers.add(marker)
        steps = report.get("steps")
        if not isinstance(steps, list):
            steps = []
            insufficient.append(f"{here}: missing/invalid steps")
        names = [step.get("name") if isinstance(step, dict) else None for step in steps]
        if names not in (["admission"], list(STEPS[1:])):
            insufficient.append(f"{here}: noncanonical self-test step order")
        by_name = {}
        for ordinal, step in enumerate(steps):
            if (not isinstance(step, dict) or not isinstance(step.get("name"), str)
                    or step["name"] not in STEPS):
                insufficient.append(f"{here}/report/steps/{ordinal}: unknown/invalid step")
                continue
            name = step["name"]
            step_names.add(name)
            by_name.setdefault(name, []).append((ordinal, step))
            if outcome(step.get("outcome")) == "invalid_or_missing":
                insufficient.append(f"{here}/report/steps/{ordinal}: missing/unknown outcome")
        if "cleanup" not in by_name:
            cleanup_steps_missing += 1
        cleanup = report.get("cleanup_sessions")
        if not isinstance(cleanup, list):
            cleanup = []
            insufficient.append(f"{here}: missing/invalid cleanup_sessions")
        run_cleanup_bad = False
        for ordinal, session in enumerate(cleanup):
            key = outcome(session.get("outcome")) if isinstance(session, dict) else "invalid_or_missing"
            cleanup_counts[key] += 1
            run_cleanup_bad |= key != "passed"
            if key == "invalid_or_missing":
                insufficient.append(f"{here}/report/cleanup_sessions/{ordinal}: invalid outcome")
        run_cleanup_bad |= any(outcome(s.get("outcome")) != "passed"
                               for _, s in by_name.get("cleanup", []))
        cleanup_run_failures += run_cleanup_bad
        report_pointer = "" if provenance["layout"] == "bare_report" else pointer + "/report"
        sample_records.append({"index": index, "pointer": report_pointer, "outcome": result,
                               "elapsed": elapsed(report), "steps": by_name,
                               "skipped": skipped_steps(report, steps)})
    command = [{"run": r["index"], "pointer": r["pointer"] + "/elapsed_ms",
                "outcome": r["outcome"], **r["elapsed"]} for r in sample_records]
    timings = {"command": metric(command)}
    for name in sorted(step_names):
        observations = []
        for record in sample_records:
            steps = record["steps"].get(name, [])
            sample = {"run": record["index"], "pointer": record["pointer"] + "/steps",
                      "outcome": "invalid_or_missing", "state": "missing"}
            if len(steps) > 1:
                sample.update(state="invalid", reason="duplicate step name")
                insufficient.append(f"{label}/runs/{record['index']}: duplicate step {name!r}")
            elif steps:
                ordinal, step = steps[0]
                sample.update(pointer=f"{record['pointer']}/steps/{ordinal}/elapsed_ms",
                              outcome=outcome(step.get("outcome")), **elapsed(step))
                if ordinal in record["skipped"]:
                    sample.pop("value")
                    sample.update(state="not_run", reported_value=0, reason=SKIP_REASON)
            observations.append(sample)
        timings["step:" + name] = metric(observations)
    return {"input": provenance, "sample_count": len(runs), "markers": sorted(markers),
            "outcomes": dict(counts),
            "exit_codes": dict(exit_codes), "cleanup_session_outcomes": dict(cleanup_counts),
            "runs_with_nonpassing_cleanup": cleanup_run_failures,
            "runs_without_cleanup_step": cleanup_steps_missing, "timings": timings}


def compare(before, after):
    insufficient, incompatible = [], []
    summaries = [summarize(doc, provenance, label, insufficient, incompatible)
                 for (doc, provenance), label in zip((before, after), ("before", "after"))]
    left, right = before[0], after[0]
    reused = set(summaries[0]["markers"]) & set(summaries[1]["markers"])
    if reused:
        insufficient.append(f"{len(reused)} self-test marker(s) reused across baselines; "
                            "samples are not distinct")
    if left.get("conditions") != right.get("conditions"):
        incompatible.append("recorded experiment conditions differ")
    status = "incompatible" if incompatible else "insufficient" if insufficient else "compatible"
    changes = {field: {"before": left.get("bridge", {}).get(field),
                       "after": right.get("bridge", {}).get(field)}
               for field in BRIDGE_FIELDS
               if isinstance(left.get("bridge"), dict) and isinstance(right.get("bridge"), dict)
               and left["bridge"].get(field) != right["bridge"].get(field)
               and all(type(doc["bridge"].get(field)) in (str, bool) for doc in (left, right))}
    differences = {}
    if status == "compatible":
        for key in sorted(summaries[0]["timings"].keys() | summaries[1]["timings"].keys()):
            a, b = (summary["timings"].get(key) for summary in summaries)
            if a is None or b is None:
                continue
            differences[key] = {}
            for group in ("all", *OUTCOMES):
                x, y = (m["all"] if group == "all" else m["by_outcome"][group] for m in (a, b))
                differences[key][group] = {
                    "before_n": x["n"], "after_n": y["n"],
                    "median_delta_ms": y["median_ms"] - x["median_ms"] if x["n"] and y["n"] else None,
                }
    return {"schema_version": 1, "status": status, "insufficient": insufficient,
            "incompatible": incompatible, "bridge_changes": changes,
            "before": summaries[0], "after": summaries[1], "differences": differences,
            "limits": ["User-supplied conditions are attestations, not independently verified.",
                       "Elapsed milliseconds are Bridge observations, not model or billing time.",
                       "Timeout/failure durations are stopping points, not successful completion latency.",
                       "Unpaired descriptive samples; no significance, causality or performance gate.",
                       "No outliers removed; confirmed skipped steps retain reported zero outside measured timings.",
                       "Keep both exact input files: hashes and JSON pointers identify every sample."]}


def human(report):
    lines = [f"Saved self-test comparison: {report['status']}"]
    for label in ("before", "after"):
        side = report[label]
        lines.append(f"{label}: {side['sample_count']} samples; outcomes="
                     + json.dumps(side["outcomes"], sort_keys=True))
        lines.append("  exit codes=" + json.dumps(side["exit_codes"], sort_keys=True))
        lines.append(f"  nonpassing cleanup: {side['runs_with_nonpassing_cleanup']} runs; "
                     f"cleanup step absent: {side['runs_without_cleanup_step']}; "
                     f"session outcomes={json.dumps(side['cleanup_session_outcomes'], sort_keys=True)}")
    for key in sorted(report["before"]["timings"].keys() | report["after"]["timings"].keys()):
        parts = []
        for label in ("before", "after"):
            metric_value = report[label]["timings"].get(key)
            if metric_value is None:
                parts.append(f"{label}: absent")
                continue
            value = metric_value["all"]
            parts.append(f"{label}: n={value['n']} median={value['median_ms']}ms "
                         f"range={value['min_ms']}..{value['max_ms']}ms "
                         f"missing={value['missing']} invalid={value['invalid']} "
                         f"not_run={value['not_run']}")
        lines.append(f"{key} (all reported outcomes): " + "; ".join(parts))
    for reason in report["incompatible"] + report["insufficient"]:
        lines.append("- " + reason)
    lines += ["Bridge elapsed observations only; no model/billing time or causal claim.",
              "All samples retained. p95 requires 20 valid samples; JSON includes outcome strata and limits."]
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--json", action="store_true", help="write JSON to stdout; summary to stderr")
    args = parser.parse_args(argv)
    try:
        report = compare(read_experiment(args.before), read_experiment(args.after))
        if args.json:
            print(json.dumps(report, indent=2, allow_nan=False, ensure_ascii=True))
        print(human(report), end="", file=sys.stderr if args.json else sys.stdout)
        return 0 if report["status"] == "compatible" else 2
    except (OSError, ValueError, TypeError, RecursionError, OverflowError) as error:
        print(f"Self-test comparison error: {ascii(str(error))}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
