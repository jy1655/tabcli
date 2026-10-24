import copy
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

import ci_test_timing as timing


def details():
    return {
        "source_sha": "a" * 40, "source_tree": "b" * 40, "source_dirty": False,
        "cpu_count": 4, "probe_errors": [], "rustc": "rustc fixture", "cargo": "cargo fixture",
        "platform": {"system": "Windows", "machine": "AMD64", "release": "fixture"},
        "environment": {key: "fixture" for key in timing.ENVIRONMENT},
    }


class TimingTests(unittest.TestCase):
    def record(self, root, script, stream=None):
        output = root / "timing"
        code = timing.run(output, root, [sys.executable, "-c", script],
                          io.BytesIO() if stream is None else stream, details())
        return code, output, json.loads((output / "timings.json").read_text())

    def test_real_failed_child_preserves_output_and_exit(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}):
            root = Path(directory)
            stream = io.BytesIO()
            code, output, data = self.record(root, "import os; os.write(1, b'out\\n'); "
                                            "os.write(2, b'err\\xff\\n'); raise SystemExit(7)", stream)
            self.assertEqual(code, 7)
            self.assertEqual(data["returncode"], 7)
            self.assertEqual((output / "output.log").read_bytes(), b"out\nerr\xff\n")
            self.assertEqual(stream.getvalue(), (output / "output.log").read_bytes())
            self.assertEqual(data["observation"]["non_utf8_lines"], 1)
            self.assertTrue((output / "summary.md").is_file())

    def test_partial_line_is_streamed_before_child_exits(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}):
            root = Path(directory)
            receipt = root / "read"
            class Handshake(io.BytesIO):
                def write(self, data):
                    result = super().write(data)
                    if b"partial" in self.getvalue():
                        receipt.touch()
                    return result
            script = ("import os,time,pathlib; os.write(1,b'partial'); "
                      f"p=pathlib.Path({str(receipt)!r}); deadline=time.monotonic()+5\n"
                      "while not p.exists() and time.monotonic()<deadline: time.sleep(.01)\n"
                      "raise SystemExit(0 if p.exists() else 9)")
            code, output, data = self.record(root, script, Handshake())
            self.assertEqual(code, 0)
            self.assertEqual((output / "output.log").read_bytes(), b"partial")
            self.assertEqual(data["returncode"], 0)

    def test_failed_console_sink_drains_child_and_does_not_hide_failure(self):
        class Broken:
            def write(self, data):
                raise OSError("closed console")
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}):
            code, output, data = self.record(Path(directory),
                                            "import os; os.write(1,b'x'*200000); raise SystemExit(13)", Broken())
            self.assertEqual(code, 13)
            self.assertEqual((output / "output.log").stat().st_size, 200000)
            self.assertEqual(len(data["errors"]), 1)

    def test_spawn_failure_retains_diagnostic_artifact(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}):
            root = Path(directory)
            output = root / "out"
            self.assertEqual(timing.run(output, root, [str(root / "absent")], io.BytesIO(), details()), 1)
            data = json.loads((output / "timings.json").read_text())
            self.assertIsNone(data["returncode"])
            self.assertTrue(data["errors"])

    def test_existing_artifact_directory_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaises(FileExistsError):
                timing.run(root, root, ["must-not-run"], io.BytesIO(), details())

    def test_split_unicode_color_and_crlf_preserve_suite_attribution(self):
        observation = timing.Observation()
        lines = [
            (b"   Finished test profile [unoptimized] target(s) in 1m 02.30s\r\n", 1),
            (b"running 2 tests\r\n", 2),
            (("test unicode_" + chr(0xD55C) + " ... \x1b[32mok\x1b[0m\r\n").encode(), 5),
            (b"test second ... ignored, requires credentials\n", 6),
            (b"test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 3.10s\n", 7),
            (b"running 1 test\n", 10),
            (b"test failure ... FAILED\n", 14),
            (b"test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.00s\n", 15),
        ]
        for line, elapsed in lines:
            for byte in line:
                observation.feed(bytes([byte]), elapsed)
        self.assertEqual(observation.compilation[0]["reported_seconds"], 62.3)
        self.assertEqual([s["reported_seconds"] for s in observation.suites], [3.1, 4.0])
        self.assertEqual([t["output_interval_seconds"] for t in observation.tests], [3, 1, 4])
        self.assertEqual([t["suite"] for t in observation.tests], [0, 0, 1])

    def test_long_line_does_not_hide_later_suite_or_retain_unbounded_parser_buffer(self):
        observation = timing.Observation()
        for _ in range(20):
            observation.feed(b"x" * 65536, 0)
            self.assertLessEqual(len(observation.buffer), timing.MAX_LINE)
        observation.feed(b"\nrunning 0 tests\n", 1)
        observation.feed(b"test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s", 2, final=True)
        self.assertEqual(observation.unparsed_long_lines, 1)
        self.assertEqual(observation.suites[0]["reported_seconds"], 0)

    def test_expected_panic_and_partial_line_observation(self):
        observation = timing.Observation()
        observation.feed(b"     Running tests/sample.rs (target/debug/deps/sample.exe)\nrunning 1 test\n", 1)
        observation.feed(b"test expected_failure - should panic ... ", 2)
        observation.feed(b"ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.00s\n", 5)
        self.assertTrue(timing.suite_complete(observation.suites[0]))
        self.assertEqual(observation.tests[0]["name"], "expected_failure")
        self.assertEqual(observation.tests[0]["output_interval_seconds"], 4)
        self.assertEqual(observation.tests[0]["observed_start_to_result_seconds"], 3)

    def test_metadata_reads_checkout_sha_and_only_named_environment(self):
        with mock.patch.dict(os.environ, {"GITHUB_SHA": "workflow-sha", "SECRET_TOKEN": "never-record"}), \
             mock.patch.object(timing, "probe", side_effect=[" M source.rs", "checkout-sha", "tree-sha", "rustc", "cargo"]) as probe:
            data = timing.metadata(Path.cwd())
        self.assertEqual(data["source_sha"], "checkout-sha")
        self.assertTrue(data["source_dirty"])
        self.assertEqual(probe.call_args_list[1].args[0], ["git", "rev-parse", "HEAD"])
        self.assertNotIn("never-record", json.dumps(data))
        self.assertEqual(data["environment"]["GITHUB_SHA"], "workflow-sha")

    def sample(self):
        return {"schema": 1, "status": "completed", "metadata": details(), "command": timing.COMMAND,
                "returncode": 0, "elapsed_seconds": 12.0, "errors": [],
                "observation": {"suites": [{"ordinal": 0, "target": "unittests src/main.rs",
                                            "parsed_tests": 1, "passed": 1, "failed": 0,
                                            "ignored": 0, "reported_seconds": 10.0}],
                                "completed_tests": [], "cargo_finished": [],
                                "unparsed_long_lines": 0, "non_utf8_lines": 0}}

    def test_comparison_distinguishes_conditions_counts_and_failed_or_partial_runs(self):
        before = self.sample()
        self.assertIn("Recorded conditions match", timing.compare(before, copy.deepcopy(before)))
        mutations = [
            lambda d: d["metadata"]["environment"].update(ImageVersion="other"),
            lambda d: d["metadata"]["environment"].update(ImageVersion=None),
            lambda d: d.update(returncode=101),
            lambda d: d["observation"]["suites"][0].update(passed=2),
            lambda d: d["observation"]["suites"][0].update(reported_seconds=None),
        ]
        for mutate in mutations:
            after = copy.deepcopy(before)
            mutate(after)
            report = timing.compare(before, after)
            self.assertIn("Conditions differ or are unknown", report)
            self.assertNotIn("Recorded conditions match", report)

    def test_suite_comparison_survives_a_new_target_and_reordered_ordinals(self):
        before = self.sample()
        after = copy.deepcopy(before)
        old = after["observation"]["suites"][0]
        old.update(ordinal=1, reported_seconds=7)
        new = {**copy.deepcopy(old), "target": "tests/new_policy.rs", "ordinal": 0,
               "reported_seconds": 1}
        after["observation"]["suites"].insert(0, new)
        result = timing.compare(before, after)
        self.assertIn("unittests src/main.rs: 10.000s -> 7.000s", result)
        self.assertIn("no libtest total delta", result)
        self.assertNotIn("Reported libtest total:", result)

    def test_capture_starts_before_child_and_final_write_failure_preserves_child_exit(self):
        original = timing.save_record
        for exit_code in [0, 19]:
            with self.subTest(exit_code=exit_code), tempfile.TemporaryDirectory() as directory, \
                 mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}):
                root = Path(directory)
                record = root / "timing" / "timings.json"
                script = ("import json,pathlib; "
                          f"d=json.loads(pathlib.Path({str(record)!r}).read_text()); "
                          "assert d['status']=='running' and d['returncode'] is None; "
                          f"raise SystemExit({exit_code})")
                def save(path, data):
                    if data["status"] == "completed":
                        raise OSError("injected final write failure")
                    original(path, data)
                with mock.patch.object(timing, "save_record", side_effect=save):
                    code, output, data = self.record(root, script)
                self.assertEqual(code, exit_code or 1)
                self.assertEqual(data["status"], "running")

    def test_native_failure_exit_codes(self):
        self.assertEqual(timing.shell_exit_code(0xC0000005, windows=True), -1073741819)
        self.assertEqual(timing.shell_exit_code(-15, windows=False), 143)
        self.assertEqual(timing.shell_exit_code(101, windows=True), 101)

    def test_missing_test_line_marks_observation_incomplete(self):
        data = self.sample()
        data["observation"]["suites"][0]["parsed_tests"] = 0
        self.assertFalse(timing.suite_complete(data["observation"]["suites"][0]))
        self.assertIn("incomplete observation", timing.compare(self.sample(), data))

    def test_unfinished_and_duplicate_targets_do_not_get_duration_totals(self):
        data = self.sample()
        data.update(status="running", returncode=None)
        report = timing.compare(self.sample(), data)
        self.assertIn("no command duration delta", report)
        self.assertNotIn("Command including compilation:", report)
        data = self.sample()
        data["observation"]["suites"] *= 2
        report = timing.compare(data, data)
        self.assertIn("incomplete observation", report)
        self.assertNotIn("Reported libtest total:", report)
        self.assertNotIn("unittests src/main.rs:", report)

    def test_invalid_records_do_not_produce_comparisons(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "record.json"
            for value in [None, {}, {**self.sample(), "schema": True},
                          {**self.sample(), "elapsed_seconds": float("nan")},
                          {**self.sample(), "elapsed_seconds": -1}]:
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaises(ValueError):
                    timing.read_record(path)


if __name__ == "__main__":
    unittest.main()
