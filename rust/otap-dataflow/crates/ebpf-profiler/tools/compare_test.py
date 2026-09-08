# Copyright The OpenTelemetry Authors
# SPDX-License-Identifier: Apache-2.0

"""Non-privileged tests for the comparison harness, without profiling a host."""

import argparse
import itertools
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import compare


class ComparisonTests(unittest.TestCase):
    # Scenario: Baseline commands are supplied as argv arrays, not shell text.
    # Guarantees: The harness rejects ambiguous command strings and empty lists.
    def test_explicit_argv_only(self):
        self.assertEqual(compare.command_array('["example", "--duration", "1s"]'),
                         ["example", "--duration", "1s"])
        for value in ['"example | another"', "[]", "[1]", "{}"]:
            with self.assertRaises(argparse.ArgumentTypeError):
                compare.command_array(value)

    # Scenario: A plan is requested without granting host-profiling consent.
    # Guarantees: It lists all native sharding/pressure cases without running them.
    def test_plan_does_not_execute_commands(self):
        completed = subprocess.run(
            [sys.executable, str(Path(compare.__file__)), "--plan-only"],
            check=True, capture_output=True, text=True,
        )
        cases = json.loads(completed.stdout)
        self.assertEqual([case[0] for case in cases],
                         ["rust-single", "rust-per-numa", "rust-fixed-2", "rust-slow-consumer"])
        self.assertTrue(all("cargo" not in case[1] for case in cases))

    # Scenario: An ordinary child emits a normalized summary and exits normally.
    # Guarantees: Per-child RSS, CPU, exit status and output metrics are captured.
    def test_measurement_records_success(self):
        program = (
            "import json,time; time.sleep(0.25); "
            "print(json.dumps({'schema':'otel-ebpf-profiler-summary-v1','samples_consumed':3}))"
        )
        with tempfile.TemporaryDirectory() as directory:
            result = compare.measure("fixture", [sys.executable, "-c", program],
                                     Path(directory), 1, 0)
            self.assertEqual(result["exit_code"], 0)
            self.assertFalse(result["timed_out"])
            self.assertGreater(result["peak_rss_kib"], 0)
            self.assertEqual(result["profiler"]["samples_consumed"], 3)
            self.assertGreater(result["samples_per_second"], 0)

    # Scenario: The deadline expires while the harness's own child is running.
    # Guarantees: It terminates that child's isolated process group and reaps it.
    def test_timeout_reaps_owned_process(self):
        ticks = itertools.count(step=30)
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(compare.time, "monotonic", side_effect=lambda: next(ticks)):
                result = compare.measure(
                    "timeout", [sys.executable, "-c", "import time; time.sleep(60)"],
                    Path(directory), 1, 0,
                )
            self.assertTrue(result["timed_out"])
            self.assertNotEqual(result["exit_code"], 0)


if __name__ == "__main__":
    unittest.main()
