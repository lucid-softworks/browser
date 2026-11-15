#!/usr/bin/env python3
"""Regression tests for the gate's treatment of failures, timeouts and missing coverage."""
import importlib.util
import json
from pathlib import Path
import tempfile
import sys
import unittest

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("compare_wpt", Path(__file__).with_name("compare-wpt.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class ComparisonTests(unittest.TestCase):
    def test_existing_failures_and_improvements_do_not_regress(self):
        base = {"/test.html": ("OK", {"works": "PASS", "known": "FAIL"})}
        self.assertEqual(gate.compare(base, base), [])
        self.assertEqual(gate.compare(base, {"/test.html": ("OK", {"works": "PASS", "known": "PASS"})}), [])

    def test_pass_to_fail_and_fail_to_timeout_regress(self):
        base = {"/test.html": ("OK", {"works": "PASS", "known": "FAIL"})}
        head = {"/test.html": ("TIMEOUT", {"works": "FAIL", "known": "TIMEOUT"})}
        self.assertEqual(len(gate.compare(base, head)), 3)

    def test_new_crash_is_not_hidden_by_existing_harness_failure(self):
        self.assertTrue(gate.compare({"/t": ("ERROR", {})}, {"/t": ("CRASH", {})}))

    def test_missing_failed_subtest_and_missing_test_regress(self):
        base = {"/one": ("OK", {"known": "FAIL"}), "/two": ("OK", {})}
        self.assertEqual(len(gate.compare(base, {"/one": ("OK", {})})), 2)

    def test_recovered_harness_can_discover_failing_subtests(self):
        self.assertEqual(gate.compare({"/t": ("TIMEOUT", {})}, {"/t": ("OK", {"now runs": "FAIL"})}), [])

    def test_new_failure_in_previously_complete_test_needs_review(self):
        self.assertTrue(gate.compare({"/t": ("OK", {"old": "PASS"})},
                                     {"/t": ("OK", {"old": "PASS", "new": "FAIL"})}))

    def test_reject_missing_empty_truncated_and_duplicate_reports(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            meta = {"exit_code": 0, "requested_tests": ["/test.html"],
                    "engine_revision": "base", "wpt_revision": "pinned"}
            (directory / "run.json").write_text(json.dumps(meta))
            result = {"test": "/test.html", "status": "OK", "subtests": [{"name": "works", "status": "PASS"}]}
            def check(report, valid=False):
                (directory / "report.json").write_text(json.dumps(report))
                if valid:
                    self.assertEqual(len(gate.load_run(directory, {"/test.html"})[1]), 1)
                else:
                    with self.assertRaises(ValueError):
                        gate.load_run(directory, {"/test.html"})
            check({"time_end": 1, "results": [result]}, valid=True)
            check({"time_end": 1, "results": []})
            check({"results": [result]})
            check({"time_end": 1, "results": [result, result]})
            check({"time_end": 1, "results": [{**result, "test": "/different.html"}]})
            (directory / "report.json").write_text('{"results":')
            with self.assertRaises(ValueError):
                gate.load_run(directory, {"/test.html"})

    def test_wall_timeout_is_failure_even_with_completed_looking_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            (directory / "run.json").write_text(json.dumps({"exit_code": 124}))
            with self.assertRaisesRegex(ValueError, "cut off"):
                gate.load_run(directory, {"/test.html"})

    def test_query_variants_share_selected_file_but_keep_distinct_outcomes(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            meta = {"exit_code": 1, "requested_tests": ["/test.html"]}
            (directory / "run.json").write_text(json.dumps(meta))
            (directory / "report.json").write_text(json.dumps({"time_end": 1, "results": [
                {"test": "/test.html?first", "status": "OK"},
                {"test": "/test.html?second", "status": "TIMEOUT"}]}))
            self.assertEqual(len(gate.load_run(directory, {"/test.html"})[1]), 2)


if __name__ == "__main__":
    unittest.main()
