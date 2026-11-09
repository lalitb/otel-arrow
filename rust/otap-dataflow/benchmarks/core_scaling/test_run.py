# Copyright The OpenTelemetry Authors
# SPDX-License-Identifier: Apache-2.0
"""Contract tests for core-scaling measurement interpretation."""
import unittest
from pathlib import Path
import tempfile
from unittest.mock import patch

import run


class HarnessTests(unittest.TestCase):
    # Scenario: snapshots contain sink, source, and unrelated pipeline counters.
    # Guarantees: only the selected group's sink items contribute to delivery.
    def test_delivered_scopes_sink_counters(self):
        def metric(group, node, value):
            return {"name": "node.input", "attributes": {
                "pipeline.group.id": {"String": group},
                "pipeline.id": {"String": "p0"}, "node.id": {"String": node}},
                "metrics": {"items": value}}
        snapshot = {"metric_sets": [metric("s", "sink", 100),
                                    metric("s", "router", 200),
                                    metric("other", "sink", 300)]}
        self.assertEqual(run.delivered(snapshot, 1), {"p0": 100})

    # Scenario: a selected pipeline has not published a sink item counter.
    # Guarantees: missing telemetry cannot silently become zero throughput.
    def test_missing_counter_fails(self):
        with self.assertRaisesRegex(RuntimeError, "Missing sink item counters"):
            run.delivered({"metric_sets": []}, 1)

    # Scenario: the worker exists but can run on two CPUs instead of its core.
    # Guarantees: the affinity guard rejects an unpinned worker.
    def test_shared_affinity_mask_fails(self):
        with tempfile.TemporaryDirectory() as temporary:
            task = Path(temporary) / "123"
            task.mkdir()
            (task / "comm").write_text("pipeline-s-p0-c\n")
            with patch.object(run.Path, "iterdir", return_value=iter([task])):
                with patch.object(run.os, "sched_getaffinity", return_value={2, 3},
                                  create=True):
                    with self.assertRaisesRegex(RuntimeError, "affinity"):
                        run.worker_affinity(999, [2])

    # Scenario: a worker is pinned to its configured CPU after startup.
    # Guarantees: the guard retains the real thread ID for later identity checks.
    def test_exact_affinity_passes(self):
        with tempfile.TemporaryDirectory() as temporary:
            task = Path(temporary) / "123"
            task.mkdir()
            (task / "comm").write_text("pipeline-s-p0-c\n")
            with patch.object(run.Path, "iterdir", return_value=iter([task])):
                with patch.object(run.os, "sched_getaffinity", return_value={2},
                                  create=True):
                    self.assertEqual(run.worker_affinity(999, [2]),
                                     {"pipeline-s-p0-c": 123})


    # Scenario: visible CPUs include SMT siblings and a second NUMA node.
    # Guarantees: selection uses distinct physical cores from a single node.
    def test_topology_excludes_siblings_and_other_nodes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for cpu, node, core in [(0, 0, 0), (1, 0, 0), (2, 0, 1), (3, 1, 2)]:
                path = root / f"sys/devices/system/cpu/cpu{cpu}"
                (path / "topology").mkdir(parents=True)
                (path / f"node{node}").mkdir()
                (path / "topology/physical_package_id").write_text("0")
                (path / "topology/core_id").write_text(str(core))
            with patch.object(run, "Path", side_effect=lambda path: root / path.lstrip("/")):
                with patch.object(run.os, "sched_getaffinity", return_value={0, 1, 2, 3},
                                  create=True):
                    self.assertEqual(run.select_cpus(2), ("node0", [0, 2]))
                    with self.assertRaisesRegex(RuntimeError, "3 visible physical cores"):
                        run.select_cpus(3)


if __name__ == "__main__":
    unittest.main()
