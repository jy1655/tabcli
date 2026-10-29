# Compare saved self-test experiments

`scripts/compare_self_test.py` compares two saved experiments with Python 3.9 or newer
and no dependencies. It reads JSON files only. It starts no provider, repairs no records,
and collects no usage or cost. CI Cargo timing remains the separate
[`ci_test_timing.py` workflow](testing.md#windows-ci-performance-observations).

The inputs wrap the schema 1 reports emitted by `src/native/self_test.rs`. The wrapper
records conditions that self-test does not report. They are the experiment owner's
attestations, not independent proof. Do not invent missing identities for old runs:
bare reports are accepted for observation but marked `insufficient` for comparison.

## Collect the two baselines

Plan repeated trials of the same self-test scenario before collecting data. Record the
actual resolved model and effort, provider backend and version, terminal and version,
machine/OS/architecture, permission mode, configuration, workspace contents and trust
state, isolated/ordinary state root, cold/warm preparation and timeout. Use stable labels
or hashes for workspace/configuration snapshots; do not put credentials in metadata.
`not_applicable` is an explicit value for an inapplicable control; `unknown` and `auto`
do not establish a resolved identity. Extra conditions are compared too.

For a release comparison, keep these conditions fixed and deliberately change only
the Bridge identity. Record the exact executable's SHA-256, its version, source revision
and whether that source was dirty. The script permits all four Bridge identity fields
to differ. A binary hash identifies the binary even when built from a dirty checkout;
keep the source diff separately if source-level attribution matters. If provider versions,
models or workspace scenarios change, collect a separate experiment instead.

Authenticate and review the workspace before running self-test. For example, after selecting
the exact binary and a task-specific artifact directory:

```sh
/absolute/path/to/tabcli self-test claude --workspace /reviewed/workspace \
  --terminal iterm2 --model <resolved-model> --effort high \
  --timeout-secs 120 --isolated --json > run-001.json 2> run-001.stderr
run_exit=$?
```

This collection command makes real model calls and opens a real surface. The comparison
tool does neither. Capture `run_exit` immediately, retain stdout/stderr on failures, and
inspect cleanup and the surface before the next attempt. Never resend an uncertain
delivery automatically. Isolated mode isolates Bridge records, not provider trust or
caches. Cold/warm describes deliberate preparation; the comparator cannot establish it.
Consider alternating baseline order to reduce drift, and record load/cache limitations.
Use the same declared trial plan and include every attempt; do not stop only after a pass.
Collect distinct runs for each baseline, including an A/A control experiment. Copying
the same run into both baselines does not supply independent observations.

Create one wrapper per baseline, replacing every example value with the observed value.
Copy `provider_version` exactly from the self-test report, including CLI labels such as
`2.1.309 (Claude Code)` or `codex-cli 0.165.0`. The comparator checks the raw string;
reducing it to the numeric version makes the wrapper disagree with the unchanged report.

```json
{
  "schema_version": 1,
  "conditions": {
    "provider": "claude",
    "provider_version": "2.1.309 (Claude Code)",
    "provider_backend": "record-the-configured-backend",
    "terminal": "iterm2",
    "terminal_version": "record-the-installed-version",
    "os": "macOS",
    "os_version": "record-the-full-version",
    "arch": "arm64",
    "machine": "stable-machine-and-resource-label",
    "model": "record-the-resolved-model",
    "effort": "high",
    "permission_mode": "default",
    "start_state": "warm: record-the-exact-preparation",
    "isolated": true,
    "workspace_scenario": "workspace-content-hash-and-trust-state",
    "configuration": "relevant-provider-and-Bridge-config-hash",
    "scenario": "schema-1-marker-roundtrip; timeout=120s; declared-trial-plan"
  },
  "bridge": {
    "version": "0.4.29",
    "binary_sha256": "replace-with-64-lowercase-hex-digits",
    "source_revision": "record-the-exact-source-revision",
    "source_dirty": false
  },
  "runs": []
}
```

Insert each report unchanged into `runs`, with the observed exit code:
`{"exit_code": 0, "report": <the parsed run-001.json object>}`. For a crash or invalid
stdout with no report use `{"exit_code": 1, "report": null}` and retain its stdout/stderr.
That attempt remains in the sample count and makes comparison insufficient. A short
Python snippet can build a wrapper without retyping a report:

```python
import json
from pathlib import Path

path = Path("before.json")  # Start with the completed metadata and an empty runs array.
experiment = json.loads(path.read_text(encoding="utf-8"))
experiment["runs"].append({
    "exit_code": 0,  # Use the actual captured run_exit, including nonzero exits.
    "report": json.loads(Path("run-001.json").read_text(encoding="utf-8")),
})
path.write_text(json.dumps(experiment, indent=2), encoding="utf-8")
```

Reports can disclose local paths and session identities. Keep source files in the same
private artifact destination as other verification evidence. Keep both exact wrapper
files alongside the comparison: its SHA-256, byte count, path and per-sample JSON pointers
provide exact input provenance. The comparator does not modify or copy its inputs.

## Run and interpret

```sh
python3 -I scripts/compare_self_test.py before.json after.json
python3 -I scripts/compare_self_test.py before.json after.json --json \
  > comparison.json 2> comparison.txt
```

Exit 0 means compatible recorded conditions, even when self-tests failed. Exit 2 means
incompatible or insufficient conditions; descriptive observations remain available but
no time deltas are produced. Exit 1 means an unreadable, malformed or oversized input;
stderr explains the input error and no comparison JSON is produced. Never interpret an
empty redirected JSON file as a successful experiment.

The report retains every run, original outcome count (`passed`, `failed`, `timed_out`,
`unsupported`, `not_verified`), command exit counts, cleanup session outcomes and runs
with nonpassing cleanup. Missing reports/outcomes are explicit. Repeated marker identities
within or across baselines prevent comparison; all samples remain in the report. Unknown
schemas/steps, noncanonical step order and duplicate step names also prevent comparison.
A missing cleanup step
is counted separately: admission refusals legitimately have only an admission step.
Early failures may lack a discovered terminal/version; explicit collection metadata is
still required and any non-null conflicting report identity prevents comparison.

Command and step time distributions include all reported outcomes and separate outcome
strata, with each value's source pointer. Missing/null times, invalid values and confirmed
`not_run` steps are counted separately, never filled with zero. Only integer milliseconds
from zero through the report's `u128` maximum are accepted. Booleans, strings, negatives,
fractional or integral-looking floats, infinity and larger integers are invalid. Integer
tokens exceeding 39 digits are classified without integer conversion, independently of
Python's global digit limit. Invalid elapsed observations retain their run and source
pointer; their raw token remains in the exact hashed input instead of the output JSON.

For schema 1, skip inference requires the complete source order `ask`, `initial_result`,
`tell`, `follow_up_result`, `cleanup`. Only the three gated middle steps can be inferred
`not_run`: the previous outcome must be known and nonpassing, and the step must have
`not_verified`, integer `elapsed_ms: 0`, explicit null request/event addresses, and the
exact source reason `not run because a preceding step was not verified`. These samples
keep their original outcome, pointer and `reported_value: 0` but do not enter measured
distributions or deltas. Admission-only refusals remain valid reports with absent later
steps. Unknown schemas or malformed order supply no skip inference and withhold all
comparisons. A reported measured 0ms remains valid, including a `not_verified` step that
does not have this skip evidence. No outlier is removed. Unequal sample counts remain explicit.

Min/median/mean/max are descriptive observations. A single sample and fewer than five valid
samples receive explicit limits. Nearest-rank p95 appears only with at least 20 valid
samples in that outcome stratum. This threshold does not establish statistical confidence.
Deltas compare unpaired medians with both sample counts; they are not a significance test,
regression gate or causal claim. Mixed-outcome deltas can reflect different failure rates;
inspect outcome strata and cleanup before interpreting them.
Timeout and failure durations are observed stopping points, not successful completion
latencies; a faster failure is not evidence of an improvement.

These are Bridge-observed elapsed times, including provider waits, terminal work,
discovery, diagnostics and cleanup as recorded. They are neither model execution time nor
billing/token usage. Hosted load, provider service load and cache state can remain uncontrolled
even when recorded conditions agree. The tool accepts at most 8 MiB per input, 1,000 runs
per baseline and 64 steps/cleanup entries per run. Duplicate JSON keys and non-JSON numeric
constants are rejected. It is a bounded developer utility, not a benchmark runner.

Focused regression checks:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p test_compare_self_test.py -v
```
