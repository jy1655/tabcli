import contextlib
import copy
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import compare_self_test as comparison


def report(index=0, result="passed", milliseconds=100):
    return {
        "schema_version": 1, "bridge_version": "0.4.29", "provider": "claude",
        "provider_version": "2.1.309", "terminal": "iterm2", "isolated": True,
        "state_root": "/fixture/state", "marker": f"AB_fixture_{index}",
        "session": f"session-fixture-{index}", "session_state": "closed", "outcome": result,
        "elapsed_ms": milliseconds,
        "steps": [{"name": name, "outcome": result, "elapsed_ms": milliseconds,
                   "reason": None, "request_address": None, "event_address": None}
                  for name in comparison.STEPS if name != "admission"],
        "cleanup_sessions": [{"session": f"session-fixture-{index}", "session_state": "closed",
                              "outcome": "passed", "reason": None}],
    }


def experiment(count=1, arm="before"):
    conditions = {field: "fixture" for field in comparison.CONDITIONS}
    conditions.update(provider="claude", provider_version="2.1.309", terminal="iterm2",
                      isolated=True)
    return {"schema_version": 1, "conditions": conditions,
            "bridge": {"version": "0.4.29", "binary_sha256": "a" * 64,
                       "source_revision": "b" * 40, "source_dirty": False},
            "runs": [{"exit_code": 0, "report": report(f"{arm}_{i}", milliseconds=i + 1)}
                     for i in range(count)]}


def saved(document):
    return document, {"path": "/fixture/experiment.json", "layout": "experiment",
                      "size_bytes": 123, "sha256": "c" * 64}


class CompareSelfTestTests(unittest.TestCase):
    def compare(self, left=None, right=None):
        left = experiment() if left is None else left
        if right is None:
            # Separate fixture runs under the same controls, with equal default timings.
            right = experiment(len(left["runs"]), arm="after")
            right["conditions"] = copy.deepcopy(left["conditions"])
            right["bridge"] = copy.deepcopy(left["bridge"])
        return comparison.compare(saved(left), saved(right))

    def test_equal_conditions_and_samples_have_zero_descriptive_delta(self):
        result = self.compare(experiment(5))
        self.assertEqual(result["status"], "compatible")
        self.assertEqual(result["differences"]["command"]["all"], {
            "before_n": 5, "after_n": 5, "median_delta_ms": 0})
        self.assertEqual(result["before"]["timings"]["command"]["all"]["median_ms"], 3)
        self.assertEqual(result["bridge_changes"], {})
        self.assertIn("not model or billing time", " ".join(result["limits"]))

    def test_release_intervention_can_change_all_bridge_identity_fields(self):
        before, after = experiment(), experiment(arm="after")
        after["bridge"] = {"version": "0.4.30", "binary_sha256": "d" * 64,
                           "source_revision": "e" * 40, "source_dirty": True}
        after["runs"][0]["report"]["bridge_version"] = "0.4.30"
        result = self.compare(before, after)
        self.assertEqual(result["status"], "compatible")
        self.assertEqual(set(result["bridge_changes"]), set(comparison.BRIDGE_FIELDS))

    def test_every_required_condition_and_identity_is_needed(self):
        for group, fields in (("conditions", (*comparison.CONDITIONS, "isolated")),
                              ("bridge", comparison.BRIDGE_FIELDS)):
            for field in fields:
                with self.subTest(group=group, field=field):
                    left = experiment()
                    del left[group][field]
                    result = self.compare(left)
                    self.assertEqual(result["status"], "insufficient")
                    self.assertEqual(result["differences"], {})

    def test_unknown_controls_and_new_extra_controls_are_not_ignored(self):
        for field in comparison.CONDITIONS:
            with self.subTest(field=field):
                value = experiment()
                value["conditions"][field] = "unknown"
                self.assertNotEqual(self.compare(value)["status"], "compatible")
        before, after = experiment(), experiment(arm="after")
        after["conditions"]["additional_control"] = "different"
        self.assertEqual(self.compare(before, after)["status"], "incompatible")

    def test_changed_environment_and_conflicting_report_block_deltas(self):
        for field in comparison.CONDITIONS:
            with self.subTest(field=field):
                before, after = experiment(), experiment(arm="after")
                after["conditions"][field] = "changed"
                result = self.compare(before, after)
                self.assertEqual(result["status"], "incompatible")
                self.assertEqual(result["differences"], {})
        value = experiment()
        value["runs"][0]["report"]["provider_version"] = "different"
        self.assertEqual(self.compare(value)["status"], "incompatible")

    def test_all_failures_and_nonpassing_cleanup_stay_in_denominators(self):
        value = experiment(5)
        for run, outcome in zip(value["runs"], comparison.OUTCOMES):
            run["report"]["outcome"] = outcome
            run["exit_code"] = 0 if outcome == "passed" else 1
        value["runs"][2]["report"]["steps"][-1]["outcome"] = "not_verified"
        value["runs"][3]["report"]["cleanup_sessions"][0]["outcome"] = "failed"
        result = self.compare(value)
        self.assertEqual(result["status"], "compatible")
        side = result["before"]
        self.assertEqual(side["sample_count"], 5)
        self.assertTrue(all(side["outcomes"][key] == 1 for key in comparison.OUTCOMES))
        self.assertEqual(side["exit_codes"], {"0": 1, "1": 4})
        self.assertEqual(side["runs_with_nonpassing_cleanup"], 2)
        self.assertEqual(side["timings"]["command"]["all"]["n"], 5)
        self.assertEqual(side["timings"]["command"]["by_outcome"]["timed_out"]["n"], 1)

    def test_invalid_and_missing_numbers_are_explicit_and_never_zero_filled(self):
        numbers = [None, -1, True, "12", float("inf"), float("nan"), 2 ** 129,
                   1.5, 1.0, 0, 2]
        value = experiment(len(numbers))
        for run, number in zip(value["runs"], numbers):
            run["report"]["elapsed_ms"] = number
        del value["runs"][1]["report"]["elapsed_ms"]
        value["runs"][0]["report"]["steps"][0]["elapsed_ms"] = False
        result = self.compare(value)
        times = result["before"]["timings"]["command"]
        self.assertEqual((times["all"]["n"], times["all"]["missing"], times["all"]["invalid"]),
                         (2, 2, 7))
        self.assertEqual(times["all"]["median_ms"], 1)
        self.assertEqual(result["before"]["timings"]["step:ask"]["all"]["invalid"], 1)
        json.dumps(result, allow_nan=False)

    def test_source_skips_preserve_outcome_and_zero_but_are_not_measured_times(self):
        value = experiment(2)
        value["runs"][0].update(exit_code=1)
        value["runs"][0]["report"]["outcome"] = "failed"
        steps = value["runs"][0]["report"]["steps"]
        steps[0]["outcome"] = "failed"
        for step in steps[1:4]:
            step.update(outcome="not_verified", elapsed_ms=0, reason=comparison.SKIP_REASON)
        # A measured not_verified step after a passed gate can legitimately take 0ms.
        measured = value["runs"][1]["report"]["steps"][2]
        measured.update(outcome="not_verified", elapsed_ms=0)
        value["runs"][1].update(exit_code=1)
        value["runs"][1]["report"]["outcome"] = "not_verified"
        value["runs"][1]["report"]["steps"][3].update(
            outcome="not_verified", elapsed_ms=0, reason=comparison.SKIP_REASON)
        steps[-1]["elapsed_ms"] = 0  # Cleanup is measured independently of prior failures.
        result = self.compare(value)
        self.assertEqual(result["status"], "compatible")
        tell = result["before"]["timings"]["step:tell"]
        self.assertEqual((tell["all"]["n"], tell["all"]["not_run"]), (1, 1))
        self.assertEqual(tell["all"]["median_ms"], 0)
        self.assertEqual(tell["by_outcome"]["not_verified"]["not_run"], 1)
        self.assertEqual(tell["samples"][0], {
            "run": 0, "pointer": "/runs/0/report/steps/2/elapsed_ms",
            "outcome": "not_verified", "state": "not_run", "reported_value": 0,
            "reason": comparison.SKIP_REASON})
        self.assertEqual(tell["samples"][1]["state"], "valid")
        self.assertEqual(result["before"]["timings"]["step:cleanup"]["all"]["n"], 2)
        self.assertIn("not_run=1", comparison.human(result))

    def test_skip_inference_requires_source_schema_order_gate_and_exact_shape(self):
        for mutation in ("schema", "reordered", "unknown_step", "truncated", "duplicate",
                         "unknown_gate", "passed_gate", "reason", "request", "event",
                         "missing_address", "boolean_zero", "float_zero"):
            with self.subTest(mutation=mutation):
                value = experiment()
                record = value["runs"][0]["report"]
                steps = record["steps"]
                steps[0]["outcome"] = "failed"
                steps[1].update(outcome="not_verified", elapsed_ms=0, reason=comparison.SKIP_REASON)
                if mutation == "schema":
                    record["schema_version"] = 2
                elif mutation == "reordered":
                    steps[0], steps[1] = steps[1], steps[0]
                elif mutation == "unknown_step":
                    steps[3]["name"] = "unknown"
                elif mutation == "truncated":
                    steps.pop()
                elif mutation == "duplicate":
                    steps.append(copy.deepcopy(steps[0]))
                elif mutation == "unknown_gate":
                    steps[0]["outcome"] = "unknown"
                elif mutation == "passed_gate":
                    steps[0]["outcome"] = "passed"
                elif mutation == "reason":
                    steps[1]["reason"] += "?"
                elif mutation in ("request", "event"):
                    steps[1][mutation + "_address"] = "observed-address"
                elif mutation == "missing_address":
                    del steps[1]["request_address"]
                else:
                    steps[1]["elapsed_ms"] = False if mutation == "boolean_zero" else 0.0
                result = self.compare(value)
                times = result["before"]["timings"]["step:initial_result"]["all"]
                self.assertEqual(times["not_run"], 0)
                if mutation in ("schema", "reordered", "unknown_step", "truncated",
                                "duplicate", "unknown_gate"):
                    self.assertEqual(result["status"], "insufficient")
                    self.assertEqual(result["differences"], {})

    def test_distribution_limits_keep_outliers_and_show_unbalanced_sample_sizes(self):
        before, after = experiment(20), experiment(1, arm="after")
        before["runs"][-1]["report"]["elapsed_ms"] = 1000000
        result = self.compare(before, after)
        a = result["before"]["timings"]["command"]["all"]
        b = result["after"]["timings"]["command"]["all"]
        self.assertEqual((a["max_ms"], a["p95_ms"]), (1000000, 19))
        self.assertIsNone(b["p95_ms"])
        self.assertEqual(b["sample_limit"], "single_sample")
        self.assertEqual(result["differences"]["command"]["all"]["after_n"], 1)
        self.assertEqual(self.compare(experiment(0))["status"], "insufficient")

    def test_missing_and_duplicate_steps_do_not_bias_distributions(self):
        value = experiment(2)
        value["runs"][0]["report"]["steps"] = []
        value["runs"][1]["report"]["steps"].append(copy.deepcopy(
            value["runs"][1]["report"]["steps"][0]))
        result = self.compare(value)
        self.assertEqual(result["status"], "insufficient")
        times = result["before"]["timings"]["step:ask"]["all"]
        self.assertEqual((times["n"], times["missing"], times["invalid"]), (0, 1, 1))

    def test_admission_failure_and_early_unknowns_remain_reportable(self):
        value = experiment()
        record = value["runs"][0]["report"]
        record.update(outcome="failed", provider_version=None, terminal=None, cleanup_sessions=[])
        record["steps"] = [{"name": "admission", "outcome": "failed", "elapsed_ms": 1}]
        value["runs"][0]["exit_code"] = 1
        result = self.compare(value)
        self.assertEqual(result["status"], "compatible")
        self.assertEqual(result["before"]["runs_without_cleanup_step"], 1)
        self.assertEqual(result["before"]["timings"]["step:ask"]["all"]["missing"], 1)

    def test_missing_report_and_unknown_outcomes_stay_visible(self):
        for invalid in (None, {}, [], {"outcome": "new_outcome"}):
            with self.subTest(report=invalid):
                value = experiment()
                value["runs"][0]["report"] = invalid
                result = self.compare(value)
                self.assertNotEqual(result["status"], "compatible")
                self.assertEqual(result["before"]["sample_count"], 1)
                self.assertEqual(result["before"]["outcomes"]["invalid_or_missing"], 1)

    def test_malformed_report_identity_is_insufficient_without_boolean_coercion(self):
        for field, invalid in (("provider", None), ("isolated", 1),
                               ("terminal", []), ("bridge_version", None)):
            with self.subTest(field=field):
                value = experiment()
                value["runs"][0]["report"][field] = invalid
                self.assertEqual(self.compare(value)["status"], "insufficient")
        value = experiment()
        value["runs"][0]["report"] = None
        self.assertEqual(self.compare(value)["status"], "insufficient")

    def test_duplicate_run_identity_is_retained_but_not_treated_as_independent(self):
        value = experiment(2)
        value["runs"][1] = copy.deepcopy(value["runs"][0])
        result = self.compare(value)
        self.assertEqual(result["status"], "insufficient")
        self.assertEqual(result["before"]["sample_count"], 2)
        self.assertTrue(any("not distinct" in reason for reason in result["insufficient"]))

    def test_cross_baseline_marker_reuse_retains_all_runs_and_withholds_deltas(self):
        for overlap in ("all", "one"):
            with self.subTest(overlap=overlap):
                before, after = experiment(3), experiment(3, arm="after")
                if overlap == "all":
                    after = copy.deepcopy(before)
                else:
                    after["runs"][1] = copy.deepcopy(before["runs"][2])
                result = self.compare(before, after)
                self.assertEqual(result["status"], "insufficient")
                self.assertEqual(result["differences"], {})
                self.assertEqual(result["before"]["sample_count"], 3)
                self.assertEqual(result["after"]["timings"]["command"]["all"]["n"], 3)
                self.assertTrue(any("reused across baselines" in reason
                                    for reason in result["insufficient"]))

    def test_unrecognized_step_names_cannot_expand_the_metric_space(self):
        value = experiment()
        value["runs"][0]["report"]["steps"] = [
            {"name": f"untrusted_{i}", "outcome": "failed", "elapsed_ms": 1}
            for i in range(comparison.MAX_STEPS)]
        result = self.compare(value)
        self.assertEqual(result["status"], "insufficient")
        self.assertEqual(len(result["before"]["timings"]), len(comparison.STEPS) + 1)

    def test_saved_exact_provenance_and_bare_legacy_report(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "legacy.json"
            raw = json.dumps(report()).encode()
            path.write_bytes(raw)
            loaded = comparison.read_experiment(path)
            result = comparison.compare(loaded, loaded)
            self.assertEqual(result["status"], "insufficient")
            self.assertEqual(result["before"]["input"]["sha256"], hashlib.sha256(raw).hexdigest())
            self.assertEqual(result["before"]["input"]["size_bytes"], len(raw))
            sample = result["before"]["timings"]["command"]["samples"][0]
            self.assertEqual(sample["pointer"], "/elapsed_ms")

    def test_adversarial_json_fails_cleanly_without_launching_anything(self):
        payloads = [b'{"runs":[],"runs":[]}', b'\xff', b'[]', b'{"runs":NaN}',
                    b'[' * 2000 + b'0' + b']' * 2000]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bad.json"
            for raw in payloads:
                with self.subTest(raw=raw[:20]):
                    path.write_bytes(raw)
                    with contextlib.redirect_stderr(io.StringIO()) as errors:
                        self.assertEqual(comparison.main([str(path), str(path), "--json"]), 1)
                    self.assertNotIn("Traceback", errors.getvalue())

    def test_limits_are_enforced_before_comparison(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "large.json"
            for value in ({"runs": [{}] * (comparison.MAX_RUNS + 1)},
                          {"runs": [{"report": {"steps": [{}] * (comparison.MAX_STEPS + 1)}}]}):
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaises(ValueError):
                    comparison.read_experiment(path)
            path.write_bytes(b" " * (comparison.MAX_BYTES + 1))
            with self.assertRaises(ValueError):
                comparison.read_experiment(path)

    def test_overflowing_json_elapsed_is_counted_invalid_in_strict_output(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory) / f"{arm}.json" for arm in ("before", "after")]
            for arm, path in zip(("before", "after"), paths):
                path.write_text(json.dumps(experiment(arm=arm)).replace(
                    '"elapsed_ms": 1', '"elapsed_ms": 1e999'), encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()) as output, \
                    contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(comparison.main([*map(str, paths), "--json"]), 0)
            result = json.loads(output.getvalue())
            self.assertEqual(result["before"]["timings"]["command"]["all"]["invalid"], 1)
            self.assertIsNone(result["differences"]["command"]["all"]["median_delta_ms"])

    def test_integer_conversion_is_length_bounded_before_int(self):
        with mock.patch.object(comparison, "int", create=True, side_effect=AssertionError):
            self.assertIs(comparison.bounded_integer("9" * 100000), comparison.UNPARSED_INTEGER)
            self.assertIs(comparison.bounded_integer("-" + "9" * 100000),
                          comparison.UNPARSED_INTEGER)
        self.assertEqual(comparison.bounded_integer(str(comparison.U128_MAX)), comparison.U128_MAX)

    def test_long_json_integers_stay_invalid_samples_with_exact_provenance(self):
        for token in ("9" * 100000, "-" + "9" * 100000, str(comparison.U128_MAX + 1)):
            with self.subTest(digits=len(token)), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "numbers.json"
                value = experiment(2)
                value["runs"][0]["report"]["elapsed_ms"] = "replace-number"
                value["runs"][0]["report"]["steps"][0]["elapsed_ms"] = "replace-number"
                raw = json.dumps(value).replace('"replace-number"', token).encode()
                path.write_bytes(raw)
                loaded = comparison.read_experiment(path)
                result = comparison.compare(loaded, saved(experiment(2, arm="after")))
                times = result["before"]["timings"]["command"]
                self.assertEqual((times["all"]["n"], times["all"]["invalid"]), (1, 1))
                self.assertEqual(result["before"]["sample_count"], 2)
                self.assertEqual(times["samples"][0]["pointer"], "/runs/0/report/elapsed_ms")
                self.assertNotIn("value", times["samples"][0])
                ask = result["before"]["timings"]["step:ask"]
                self.assertEqual(ask["all"]["invalid"], 1)
                self.assertEqual(ask["samples"][0]["pointer"], "/runs/0/report/steps/0/elapsed_ms")
                self.assertEqual(result["before"]["input"]["sha256"], hashlib.sha256(raw).hexdigest())
                self.assertEqual(result["before"]["input"]["size_bytes"], len(raw))
                json.dumps(result, allow_nan=False)
        for value in (comparison.U128_MAX, 0):
            self.assertEqual(comparison.elapsed({"elapsed_ms": value}),
                             {"state": "valid", "value": value})

    def test_json_cli_is_machine_readable_read_only_and_keeps_human_summary_separate(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory) / f"{arm}.json" for arm in ("before", "after")]
            raw = [json.dumps(experiment(arm=arm)).encode() for arm in ("before", "after")]
            for path, contents in zip(paths, raw):
                path.write_bytes(contents)
            with contextlib.redirect_stdout(io.StringIO()) as output, \
                    contextlib.redirect_stderr(io.StringIO()) as summary:
                self.assertEqual(comparison.main([*map(str, paths), "--json"]), 0)
            self.assertEqual(json.loads(output.getvalue())["status"], "compatible")
            self.assertIn("1 samples", summary.getvalue())
            self.assertEqual([path.read_bytes() for path in paths], raw)
            self.assertEqual(set(Path(directory).iterdir()), set(paths))
            # Actual reuse is diagnosed, without rewriting the supplied identity.
            with contextlib.redirect_stdout(io.StringIO()) as output, \
                    contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(comparison.main([str(paths[0]), str(paths[0]), "--json"]), 2)
            reused = json.loads(output.getvalue())
            self.assertEqual(reused["status"], "insufficient")
            self.assertEqual(reused["differences"], {})
            self.assertEqual([path.read_bytes() for path in paths], raw)


if __name__ == "__main__":
    unittest.main()
