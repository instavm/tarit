#!/usr/bin/env python3
"""Unit tests for the lazy-fork performance gate."""

from __future__ import annotations

import importlib.util
import json
import stat
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("fork_lazy_performance.py")
SPEC = importlib.util.spec_from_file_location("fork_lazy_performance", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class ForkPerformanceTests(unittest.TestCase):
    def test_case_requires_metrics_executes_children_and_cleans_every_vm(self) -> None:
        class FakeClient:
            def __init__(self) -> None:
                self.memory_mib = 0
                self.deleted = []

            def create(self, memory_mib: int, _vcpus: int) -> str:
                self.memory_mib = memory_mib
                return "source"

            def execute(self, _vm_id: str, command: str):
                if "MemTotal" in command:
                    return {"stdout": f"{self.memory_mib * 1024}\n"}
                return {"stdout": ""}

            def fork(self, source_id: str, child_id: str):
                return {
                    "source_vm_id": source_id,
                    "vm": {"id": child_id, "status": "running"},
                    "metrics": {
                        "total_us": 100,
                        "snapshot_artifact_us": 20,
                        "child_ready_us": 70,
                        "live_snapshot": {"downtime_us": 5},
                    },
                }

            def delete(self, vm_id: str) -> None:
                self.deleted.append(vm_id)

        client = FakeClient()
        result = MODULE.run_case(client, 256, 1, 2)
        self.assertEqual(result["iterations"], 2)
        self.assertEqual(result["metrics_us"]["total_us"]["p99"], 100)
        self.assertEqual(client.deleted[-1], "source")
        self.assertEqual(len(client.deleted), 3)

    def test_hundred_sample_tail_uses_nearest_rank(self) -> None:
        values = list(range(1, 101))
        result = MODULE.summary(values, 100)
        self.assertEqual(result["p50"], 50.5)
        self.assertEqual(result["p95"], 95)
        self.assertEqual(result["p99"], 99)
        self.assertEqual(result["max"], 100)

    def test_summary_rejects_missing_and_zero_samples(self) -> None:
        with self.assertRaisesRegex(MODULE.GateFailure, "expected 100"):
            MODULE.summary([1] * 99, 100)
        with self.assertRaisesRegex(MODULE.GateFailure, "positive"):
            MODULE.summary([1] * 99 + [0], 100)

    def test_ratio_rejects_an_invalid_baseline(self) -> None:
        self.assertEqual(MODULE.bounded_ratio(125, 100), 1.25)
        with self.assertRaisesRegex(MODULE.GateFailure, "must be positive"):
            MODULE.bounded_ratio(1, 0)

    def test_report_is_private_complete_and_atomically_replaced(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "nested" / "report.json"
            MODULE.write_report(path, {"schema_version": 1, "passed": True})
            self.assertEqual(
                json.loads(path.read_text()),
                {"schema_version": 1, "passed": True},
            )
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            self.assertEqual(list(path.parent.glob(".*.tmp")), [])


if __name__ == "__main__":
    unittest.main()
