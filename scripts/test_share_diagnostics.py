"""Public-schema and hostile-input regression tests for the offline formatter."""

import copy
import io
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock

import share_diagnostics as share


CANARY = "PRIVATE_CANARY_9f731a"
HOSTILE = CANARY + "\x1b[2J\r\n```\n[link](https://private.invalid)\u202e\u2066"


def doctor():
    return {"schema_version": 1, "ok": True, "provider": "claude", "probe": False,
            "started_unix_ms": 1000, "finished_unix_ms": 1023,
            "scope": {"provider": "claude", "current_version": None,
                      "probe": False, "probe_budget_ms": 5000},
            "configured": {"provider_version_at_launch": "2.1.309 (Claude Code)"},
            "observations": {"stored_state": "working", "generation": 4,
                             "active_request_state": "pending", "recovery_required": False},
            "checks": [{"id": "turn", "availability": "unknown",
                        "reason_code": "delivery_unconfirmed"}]}


def status():
    return {"schema_version": 1, "ok": True, "incomplete": True,
            "scanned": {"sessions": 2, "listed": 1},
            "incomplete_reasons": [{"session": "session-unreadable", "reason": HOSTILE}],
            "sessions": [{"id": "session-example", "provider": "codex", "state": "ready",
                          "generation": 9, "turn_claimed": False, "recovery_required": False,
                          "unreadable_requests": 0, "held": None, "derived_from": "observation",
                          "question_observation": "unreadable",
                          "active_request": None, "latest_result": {"request_state": "completed"},
                          "owner": {"process_alive": None, "identity_matches": None},
                          "attention": ["records_partially_unreadable"],
                          "origin_observation": {"kind": "external", "ended": None,
                                                 "reason": "origin_external", "derived_from": "records"}}]}


def timeline():
    return {"schema_version": 1, "ok": True, "session": "session-example",
            "session_state": "working", "recovery_required": False, "incomplete": True,
            "unreadable_requests": 1,
            "requests": [{"request_state": "pending", "session_state": "working",
                          "bridge_observed_elapsed_ms": None,
                          "bridge_observed_elapsed_reason": "no_published_result",
                          "delivery": "unknown", "derived_from": "result"},
                         {"request_state": "cancelled", "bridge_observed_elapsed_ms": 12438,
                          "bridge_observed_elapsed_reason": None,
                          "delivery": "completion_recorded", "derived_from": "result",
                          "question_emitted": True, "question_emission_count": 2,
                          "question_observation": "incomplete"}],
            "entries": [{"stage": "completion", "record_state": "unreadable",
                         "source": "events/event-1.json", "detail": HOSTILE}],
            "session_entries": [{"stage": "launch_phase", "record_state": "missing",
                                 "source": "launch.json", "detail": None}]}


def inspect():
    return {"schema_version": 1, "ok": True, "provider": "pi", "stored_state": "closed",
            "recorded_events": 3, "configured": {"provider_version_at_launch": "1.0.4"},
            "requests": [{"bridge_observed_elapsed_ms": 0,
                          "bridge_observed_elapsed_reason": None, "active": False}]}


class ShareDiagnosticsTests(unittest.TestCase):
    def parse(self, source):
        raw = json.dumps(source, ensure_ascii=False).encode("utf-8")
        return share.summarize(share.read_document(io.BytesIO(raw)))

    def assert_private(self, summary):
        for output_format in ("json", "markdown"):
            text = share.render(summary, output_format)
            self.assertNotIn(CANARY, text)
            self.assertNotIn("https://private.invalid", text)
            self.assertNotIn("session-example", text)
            self.assertNotIn("\x1b", text)
            self.assertNotIn("\r", text)
            self.assertNotIn("\u202e", text)
            self.assertNotIn("\u2066", text)
            text.encode("ascii")
            self.assertLessEqual(len(text.encode("ascii")), share.MAX_OUTPUT_BYTES)
            if output_format == "json":
                self.assertEqual(json.loads(text), summary)
            else:
                self.assertEqual(text.count("```"), 2)

    def test_all_public_input_shapes_and_meaningful_fields(self):
        for kind, fixture in [("doctor", doctor), ("status", status),
                              ("timeline", timeline), ("inspect", inspect)]:
            with self.subTest(kind=kind):
                summary = self.parse(fixture())
                self.assertEqual(summary["source_kind"], kind)
                self.assertEqual(summary["source_schema_version"], 1)
                self.assertEqual(summary["ok"], True)
                self.assertIn("not authenticated", summary["provenance"])
                self.assert_private(summary)
        doc = self.parse(doctor())["summary"]
        self.assertEqual(doc["configured"]["provider_version_at_launch"], "2.1.309")
        self.assertEqual(doc["scope"]["unavailable_fields"]["current_version"], "null")
        self.assertEqual(doc["checks"]["items"][0]["reason_code"], "delivery_unconfirmed")
        self.assertEqual(doc["query_duration"], {"elapsed_ms": 23,
                         "derived_from": "supplied_doctor_timestamps"})
        rows = self.parse(status())["summary"]
        self.assertEqual(rows["scanned"], {"sessions": 2, "listed": 1})
        self.assertEqual(rows["incomplete_reasons"], {"input_count": 1, "content": "omitted"})
        self.assertEqual(rows["sessions"]["items"][0]["unavailable_fields"]["held"], "null")
        self.assertEqual(rows["sessions"]["items"][0]["question_observation"], "unreadable")
        requests = self.parse(timeline())["summary"]["requests"]["items"]
        self.assertEqual(requests[0]["delivery"], "unknown")
        self.assertEqual(requests[0]["bridge_observed_elapsed_reason"], "no_published_result")
        self.assertEqual(requests[1]["bridge_observed_elapsed_ms"], 12438)
        self.assertEqual(requests[1]["request_state"], "cancelled")
        self.assertEqual(requests[1]["question_emission_count"], 2)
        self.assertEqual(requests[1]["question_observation"], "incomplete")
        self.assertEqual(self.parse(timeline())["summary"]["entries"]["items"][0]["source"], "events/*")
        inspected = self.parse(inspect())["summary"]
        self.assertEqual(inspected["event_counts"]["recorded_events"], 3)
        self.assertEqual(inspected["requests"]["items"][0]["active"], False)

    def test_canaries_in_every_free_text_location_are_never_copied(self):
        names = ("prompt result message question question_emissions questions options label text "
                 "error detail next_action title workspace executable env evidence next_command "
                 "provider_session_id request_id session event_id turn_id source context_sources "
                 "request_index_error session_error hold_error model effort custom_extension").split()
        for fixture in (doctor, status, timeline, inspect):
            source = fixture()
            # Insert each sensitive key at every object, including arbitrary nested evidence.
            def poison(value):
                if type(value) is dict:
                    for child in list(value.values()):
                        poison(child)
                    for name in names:
                        if name not in value:
                            value[name] = {CANARY: HOSTILE} if name == "evidence" else HOSTILE
                    value[CANARY] = HOSTILE
                elif type(value) is list:
                    for child in value:
                        poison(child)
            poison(source)
            self.assert_private(self.parse(source))

    def test_string_canary_under_every_allowlisted_scalar_field_is_rejected(self):
        # Walk actual public-shaped fixtures; every retained scalar is attacked in place.
        for fixture in (doctor, status, timeline, inspect):
            original = fixture()
            def paths(value, prefix=()):
                if type(value) is dict:
                    for key, child in value.items():
                        yield from paths(child, prefix + (key,))
                elif type(value) is list:
                    for index, child in enumerate(value):
                        yield from paths(child, prefix + (index,))
                else:
                    yield prefix
            for path in paths(original):
                if path == ("schema_version",):
                    continue
                with self.subTest(fixture=fixture.__name__, path=path):
                    source = copy.deepcopy(original)
                    target = source
                    for key in path[:-1]:
                        target = target[key]
                    target[path[-1]] = HOSTILE
                    self.assert_private(self.parse(source))

    def test_typed_field_validation_rejects_coercion_negative_huge_and_non_enum(self):
        for bad in (True, -1, 1.5, share.MAX_INTEGER + 1, "12", HOSTILE, [], {}):
            source = timeline()
            source["requests"][1]["bridge_observed_elapsed_ms"] = bad
            item = self.parse(source)["summary"]["requests"]["items"][1]
            self.assertNotIn("bridge_observed_elapsed_ms", item)
            self.assertEqual(item["unavailable_fields"]["bridge_observed_elapsed_ms"], "invalid")
        for bad in (1, 0, "false", [], {}):
            source = status()
            source["incomplete"] = bad
            self.assertEqual(self.parse(source)["summary"]["unavailable_fields"]["incomplete"], "invalid")
        source = doctor()
        source["checks"][0].update(id="turn_" + CANARY, availability="new_state",
                                    reason_code="future_reason")
        item = self.parse(source)["summary"]["checks"]["items"][0]
        self.assertEqual(item, {"unavailable_fields": {"id": "unrecognized",
                               "availability": "unrecognized", "reason_code": "unrecognized"}})

    def test_versions_use_complete_known_forms_and_do_not_accept_embedded_secrets(self):
        for value, expected in [("0.163.0", "0.163.0"), ("codex-cli 0.163.0", "0.163.0"),
                                ("2.1.309 (Claude Code)", "2.1.309"),
                                ("agy version 1.3.0", "1.3.0")]:
            self.assertEqual(share.version(value), expected)
        for value in (HOSTILE, "0.163.0-" + CANARY, "0.163.0+" + CANARY,
                      "0.163.0\n", "prefix 0.163.0", "0.163.0 " + CANARY,
                      "\u202e0.163.0", "0.163.0\x1b[2J", "1.2", 123, True,
                      "codex-cli 1.2.3 (Claude Code)", "01.2.3"):
            self.assertIsNone(share.version(value))

    def test_embedded_numeric_version_canary_is_rejected_in_every_version_location(self):
        numeric_canary = "12345.23456.34567"
        for value in ("private-pin=" + numeric_canary, numeric_canary + " private-pin",
                      "codex-cli private-pin=" + numeric_canary,
                      "private-pin=" + numeric_canary + " (Claude Code)",
                      "agy version private-pin=" + numeric_canary):
            source = doctor()
            source["scope"]["current_version"] = value
            source["configured"]["provider_version_at_launch"] = value
            source["checks"][0]["evidence"] = {"provider_version": value}
            inspected = inspect()
            inspected["configured"]["provider_version_at_launch"] = value
            for fixture in (source, inspected):
                summary = self.parse(fixture)
                for output_format in ("json", "markdown"):
                    self.assertNotIn(numeric_canary, share.render(summary, output_format))
                self.assertEqual(summary["summary"]["configured"]["unavailable_fields"]
                                 ["provider_version_at_launch"], "invalid")
            self.assertEqual(self.parse(source)["summary"]["scope"]["unavailable_fields"]
                             ["current_version"], "invalid")

    def test_source_families_never_copy_filenames_or_hostile_paths(self):
        for value, expected in (("requests/claim-1.json", "requests/*.json"),
                                ("requests/" + CANARY + ".json", "requests/*.json"),
                                ("events/event-1.json", "events/*"),
                                ("events/" + CANARY, "events/*"),
                                ("requests/", "requests/"), ("events/", "events/"),
                                ("turn.claim", "turn.claim")):
            with self.subTest(value=value):
                source = timeline()
                source["entries"][0]["source"] = value
                summary = self.parse(source)
                self.assertEqual(summary["summary"]["entries"]["items"][0]["source"], expected)
                self.assert_private(summary)
        for bad in ("/private/requests/claim.json", "C:\\private\\events\\one.json",
                    "requests/../private.json", "requests/nested/private.json",
                    "requests/..\\private.json", "requests/.json", "requests/claim.txt",
                    "events/.", "events/..", "events/nested/private.json", "events/\\private",
                    "events/" + HOSTILE, "requests/\u202eprivate.json",
                    "events/" + "a" * 256, "unknown/" + CANARY):
            source = timeline()
            source["entries"][0]["source"] = bad
            summary = self.parse(source)
            row = summary["summary"]["entries"]["items"][0]
            self.assertNotIn("source", row)
            self.assertEqual(row["unavailable_fields"]["source"], "unrecognized")
            self.assert_private(summary)
        for value, expected in ((share.MISSING, "missing"), (None, "null"),
                                (True, "invalid"), (1, "invalid"), ({}, "invalid")):
            source = timeline()
            if value is share.MISSING:
                del source["entries"][0]["source"]
            else:
                source["entries"][0]["source"] = value
            row = self.parse(source)["summary"]["entries"]["items"][0]
            self.assertEqual(row["unavailable_fields"]["source"], expected)
        source = timeline()
        source["session_entries"] = [{"stage": "turn_claim", "source": "turn.claim",
                                      "record_state": "unreadable"}]
        self.assertEqual(self.parse(source)["summary"]["session_entries"]["items"],
                         source["session_entries"])

    def test_question_qualifiers_preserve_only_exact_request_and_session_categories(self):
        for render in (share.request, share.session):
            for value in ("incomplete", "unreadable"):
                source = {"question_observation": value, "question_emission_count": 1,
                          "question_emitted": True, "question_emissions": HOSTILE}
                summary = render(source)
                self.assertEqual(summary["question_observation"], value)
                self.assert_private(summary)
            for value, state in ((share.MISSING, "missing"), (None, "null"),
                                 ("complete", "unrecognized"), ("future", "unrecognized"),
                                 (HOSTILE, "unrecognized"), (True, "invalid"),
                                 (1, "invalid"), ({}, "invalid"), ([], "invalid")):
                source = {} if value is share.MISSING else {"question_observation": value}
                summary = render(source)
                self.assertNotIn("question_observation", summary)
                self.assertEqual(summary["unavailable_fields"]["question_observation"], state)
                self.assert_private(summary)
        source = inspect()
        source["question_observation"] = "unreadable"
        source["requests"][0]["question_observation"] = "incomplete"
        summary = self.parse(source)["summary"]
        self.assertEqual(summary["question_observation"], "unreadable")
        self.assertEqual(summary["requests"]["items"][0]["question_observation"], "incomplete")

    def test_long_valid_attention_renders_on_the_default_markdown_cli(self):
        source = status()
        attention = ["records_partially_unreadable", "request_index_incomplete",
                     "residual_surface_unverified"]
        self.assertGreater(len("+".join(attention)), 64)
        source["sessions"][0]["attention"] = attention
        summary = self.parse(source)
        self.assertEqual(summary["summary"]["sessions"]["items"][0]["attention"]["items"],
                         [{"category": value} for value in attention])
        self.assert_private(summary)
        result = subprocess.run([sys.executable, "-B", str(Path(share.__file__).resolve())],
                                input=json.dumps(source).encode(), capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, b"")
        self.assertEqual(result.stdout.splitlines()[0], b"# Agent Bridge diagnostic summary")
        for value in attention:
            self.assertIn(value.encode("ascii"), result.stdout)

    def test_current_provider_reason_categories_survive_without_evidence(self):
        for check_id, reason in [
                ("claude_messaging", "claude_runtime_gates_unverified"),
                ("session_records", "records_busy"),
                ("codex_daemon", "codex_daemon_unavailable"),
                ("agy_input_receipt", "agy_no_input_receipt"),
                ("pi_provider_credentials", "pi_provider_credentials_not_ready")]:
            source = doctor()
            source["checks"] = [{"id": check_id, "reason_code": reason,
                                 "availability": "unknown", "evidence": {"token": HOSTILE}}]
            summary = self.parse(source)
            self.assertEqual(summary["summary"]["checks"]["items"],
                             [{"id": check_id, "reason_code": reason, "availability": "unknown"}])
            self.assert_private(summary)

    def test_claude_completion_categories_preserve_no_evidence_payload(self):
        reasons = {
            "claude_completion": (
                "claude_completion_mode_unreadable", "claude_completion_mode_malformed",
                "claude_completion_mode_unrecognized", "claude_completion_mode_absent",
                "claude_completion_legacy_stop", "claude_completion_readiness_recorded",
                "claude_completion_readiness_not_observed", "claude_completion_readiness_unrecognized",
                "claude_completion_readiness_malformed", "claude_completion_readiness_unreadable",
                "session_required"),
            "claude_completion_diagnostic": (
                "claude_completion_diagnostic_recorded", "claude_completion_diagnostic_not_recorded",
                "claude_completion_diagnostic_unrecognized", "claude_completion_diagnostic_malformed",
                "claude_completion_diagnostic_unreadable", "session_required"),
        }
        for check_id, categories in reasons.items():
            for reason in categories:
                with self.subTest(check=check_id, reason=reason):
                    source = doctor()
                    source["checks"] = [{"id": check_id, "availability": "unknown",
                                         "reason_code": reason, "detail": HOSTILE,
                                         "next_action": HOSTILE,
                                         "evidence": {"mode": {"reason": HOSTILE},
                                                      "readiness": {"session_start_conversation": HOSTILE},
                                                      "latest_diagnostic": {"event": HOSTILE,
                                                                            "reason": HOSTILE}}}]
                    summary = self.parse(source)
                    self.assertEqual(summary["summary"]["checks"]["items"],
                                     [{"id": check_id, "availability": "unknown", "reason_code": reason}])
                    self.assert_private(summary)

    def test_doctor_duration_never_invents_time_or_uses_invalid_timestamps(self):
        for value, state in [(HOSTILE, "invalid"), (True, "invalid"),
                             (None, "null"), (-1, "invalid")]:
            source = doctor()
            source["started_unix_ms"] = value
            result = self.parse(source)["summary"]["query_duration"]
            self.assertEqual(result, {"unavailable_fields": {"started_unix_ms": state}})
        source = doctor()
        del source["started_unix_ms"]
        result = self.parse(source)["summary"]["query_duration"]
        self.assertEqual(result, {"unavailable_fields": {"started_unix_ms": "missing"}})
        source["started_unix_ms"] = source["finished_unix_ms"] + 1
        result = self.parse(source)["summary"]["query_duration"]
        self.assertEqual(result, {"unavailable_fields": {"elapsed_ms": "inverted_time"}})

    def test_missing_null_and_invalid_are_distinct_without_inventing_facts(self):
        source = status()
        del source["scanned"]
        source["sessions"] = None
        source["incomplete_reasons"] = "not an array"
        summary = self.parse(source)["summary"]
        self.assertEqual(summary["scanned"], {"input_state": "missing"})
        self.assertEqual(summary["sessions"], {"input_state": "null"})
        self.assertEqual(summary["incomplete_reasons"], {"input_state": "invalid"})
        source["sessions"] = [None, HOSTILE, {"state": "new_state"}]
        rows = self.parse(source)["summary"]["sessions"]["items"]
        self.assertEqual(rows[0], {"input_state": "null"})
        self.assertEqual(rows[1], {"input_state": "invalid"})
        self.assertEqual(rows[2]["unavailable_fields"]["state"], "unrecognized")
        self.assertEqual(rows[2]["unavailable_fields"]["provider"], "missing")

    def test_arrays_are_bounded_and_omitted_count_is_explicit(self):
        source = status()
        source["sessions"] *= share.MAX_ITEMS + 7
        summary = self.parse(source)
        rows = summary["summary"]["sessions"]
        self.assertEqual(rows["input_count"], share.MAX_ITEMS + 7)
        self.assertEqual(rows["included_count"], share.MAX_ITEMS)
        self.assertEqual(rows["omitted_count"], 7)
        self.assert_private(summary)
        with mock.patch.object(share, "MAX_OUTPUT_BYTES", 10):
            with self.assertRaisesRegex(share.InputError, "summary output limit exceeded"):
                share.render(summary, "json")

    def test_invalid_json_utf8_depth_duplicates_numbers_and_oversize_fail_closed(self):
        invalid = [b"", b"{", b"{} " + CANARY.encode(), b'"\xff"', b'{}\x00',
                   b'{"schema_version":1,"schema_version":1}',
                   b'{"checks":[],"x":{"a":0,"a":1}}',
                   b'{"x":NaN}', b'{"x":Infinity}', b'{"x":1e9999}',
                   b'{"x":' + b"9" * 5000 + b"}",
                   b"[" * (share.MAX_DEPTH + 1) + b"]" * (share.MAX_DEPTH + 1)]
        for raw in invalid:
            with self.subTest(raw=raw[:60]), self.assertRaises(share.InputError) as caught:
                share.read_document(io.BytesIO(raw))
            self.assertNotIn(CANARY, str(caught.exception))
        with mock.patch.object(share, "MAX_INPUT_BYTES", 10):
            stream = io.BytesIO(("\u20ac" * 20).encode("utf-8"))
            with self.assertRaisesRegex(share.InputError, "input byte limit exceeded"):
                share.read_document(stream)
            self.assertEqual(stream.tell(), 11)
        with mock.patch.object(share, "MAX_NODES", 3):
            with self.assertRaisesRegex(share.InputError, "input node limit exceeded"):
                share.read_document(io.BytesIO(b"[0,1,2]"))

    def test_unknown_schema_shape_and_ambiguous_inputs_do_not_guess(self):
        for source in ([], None, {}, {"schema_version": True}, {"schema_version": 2},
                       {"schema_version": HOSTILE},
                       {"schema_version": 1, "result": HOSTILE},
                       {**doctor(), "sessions": []}):
            with self.subTest(source=source), self.assertRaises(share.InputError):
                share.summarize(source)

    def test_multibyte_controls_bidi_and_surrogates_in_omitted_content(self):
        source = timeline()
        source["entries"][0]["detail"] = "\u20ac\U0001f512\x00" + HOSTILE + "\ud800"
        raw = json.dumps(source).encode("utf-8")
        self.assert_private(share.summarize(share.read_document(io.BytesIO(raw))))
        source["entries"][0]["detail"] = "\u20ac\U0001f512" + HOSTILE
        self.assert_private(self.parse(source))
        # Braces and escaped quotes within omitted strings do not count as nesting.
        source["entries"][0]["detail"] = '\\"' + "[" * 100 + HOSTILE
        self.assert_private(self.parse(source))

    def test_no_mutation_and_cli_only_reads_stdin_writes_stdout(self):
        source = timeline()
        source["result"] = "exact\r\noriginal\u20ac" + HOSTILE
        before = copy.deepcopy(source)
        raw = json.dumps(source).encode("utf-8")
        self.assert_private(share.summarize(source))
        self.assertEqual(source, before)
        stream = io.BytesIO(raw)
        self.assert_private(share.summarize(share.read_document(stream)))
        self.assertEqual(stream.getvalue(), raw)
        self.assertEqual(source, before)
        command = [sys.executable, "-B", str(Path(share.__file__).resolve()), "--format", "json"]
        result = subprocess.run(command, input=raw, capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, b"")
        self.assert_private(json.loads(result.stdout))
        failed = subprocess.run(command, input=b'{"secret":' + CANARY.encode(),
                                capture_output=True, timeout=5)
        self.assertEqual(failed.returncode, 2)
        self.assertEqual(failed.stdout, b"")
        self.assertNotIn(CANARY.encode(), failed.stderr)
        # The formatter has no destination option and creates no files itself.
        denied = subprocess.run(command + ["--output", HOSTILE], input=raw,
                                capture_output=True, timeout=5)
        self.assertEqual(denied.returncode, 2)
        self.assertEqual(denied.stdout, b"")
        self.assertNotIn(CANARY.encode(), denied.stderr)
        self.assertNotIn(b"\x1b", denied.stderr)
        with mock.patch("builtins.open", side_effect=AssertionError("unexpected file access")):
            self.assert_private(self.parse(doctor()))


if __name__ == "__main__":
    unittest.main()
