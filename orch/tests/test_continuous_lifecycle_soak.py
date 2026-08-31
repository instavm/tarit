#!/usr/bin/env python3
"""Unit tests for continuous lifecycle status publication."""

from __future__ import annotations

import importlib.util
import json
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace


MODULE_PATH = Path(__file__).with_name("continuous_lifecycle_soak.py")
SPEC = importlib.util.spec_from_file_location("continuous_lifecycle_soak", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class StatusPublicationTests(unittest.TestCase):
    def make_soak(self, status_file: Path):
        return MODULE.Soak(SimpleNamespace(
            api_key="key",
            base_url="http://127.0.0.1:1",
            case_name="ubuntu66",
            epoch=7,
            seed=20260831,
            status_file=str(status_file),
        ))

    def test_event_atomically_publishes_private_status(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            status_file = Path(directory) / "nested" / "status.json"
            soak = self.make_soak(status_file)
            soak.operations = 19
            soak.snapshots = 2
            soak.event("operation_complete", action="fork_anchor")

            payload = json.loads(status_file.read_text(encoding="utf-8"))
            self.assertEqual(payload["schema_version"], 1)
            self.assertEqual(payload["state"], "running")
            self.assertEqual(payload["latest_event"], "operation_complete")
            self.assertEqual(payload["operations"], 19)
            self.assertEqual(payload["snapshots"], 2)
            self.assertEqual(payload["case"], "ubuntu66")
            self.assertEqual(payload["epoch"], 7)
            self.assertEqual(payload["action"], "fork_anchor")
            self.assertEqual(payload["forks"]["count"], 0)
            self.assertEqual(stat.S_IMODE(status_file.stat().st_mode), 0o600)
            self.assertEqual(list(status_file.parent.glob(".status.*.tmp")), [])

    def test_terminal_events_publish_terminal_state(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            status_file = Path(directory) / "status.json"
            soak = self.make_soak(status_file)
            soak.event("soak_pass", minimum_anchor_age_s=3600)
            self.assertEqual(
                json.loads(status_file.read_text(encoding="utf-8"))["state"],
                "passed",
            )
            soak.event("soak_failure", error="injected")
            self.assertEqual(
                json.loads(status_file.read_text(encoding="utf-8"))["state"],
                "failed",
            )

    def test_action_summary_reports_counts_and_latency_percentiles(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            soak = self.make_soak(Path(directory) / "status.json")
            soak.action_latencies_ms = {
                "fork_anchor": [10.0, 20.0, 30.0, 40.0, 50.0],
                "pause_resume": [4.0],
            }

            self.assertEqual(soak.action_summary(), {
                "fork_anchor": {
                    "count": 5,
                    "p50_ms": 30.0,
                    "p95_ms": 50.0,
                    "p99_ms": 50.0,
                    "max_ms": 50.0,
                },
                "pause_resume": {
                    "count": 1,
                    "p50_ms": 4.0,
                    "p95_ms": 4.0,
                    "p99_ms": 4.0,
                    "max_ms": 4.0,
                },
            })

    def test_fork_metrics_are_validated_and_summarized(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            soak = self.make_soak(Path(directory) / "status.json")
            base = {
                "path": "local",
                "source_resolution_us": 10,
                "operation_claim_us": 20,
                "snapshot_artifact_us": 300,
                "child_ready_us": 400,
                "operation_commit_us": 30,
                "total_us": 800,
                "live_snapshot": {
                    "rounds": 3,
                    "pages_copied": 100,
                    "final_dirty_pages": 4,
                    "elapsed_us": 250,
                    "downtime_us": 25,
                    "termination": "converged",
                },
            }
            soak.record_fork_metrics({"metrics": base}, 1)
            second = json.loads(json.dumps(base))
            second["path"] = "cross_node"
            second["total_us"] = 1200
            second["live_snapshot"]["termination"] = "max_rounds"
            second["live_snapshot"]["downtime_us"] = 50
            soak.record_fork_metrics({"metrics": second}, 4)

            summary = soak.fork_summary()
            self.assertEqual(summary["count"], 2)
            self.assertEqual(summary["paths"], {"cross_node": 1, "local": 1})
            self.assertEqual(
                summary["terminations"], {"converged": 1, "max_rounds": 1}
            )
            self.assertEqual(summary["measurements"]["total_us"], {
                "p50": 800,
                "p95": 1200,
                "p99": 1200,
                "max": 1200,
            })
            self.assertEqual(
                summary["measurements"]["live_downtime_us"]["max"], 50
            )
            self.assertEqual(
                summary["by_source_vcpus"]["1"]["measurements"]["total_us"],
                {"p50": 800, "p95": 800, "p99": 800, "max": 800},
            )
            self.assertEqual(summary["by_source_vcpus"]["4"]["count"], 1)

    def test_fork_metrics_reject_incoherent_downtime(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            soak = self.make_soak(Path(directory) / "status.json")
            with self.assertRaises(AssertionError):
                soak.record_fork_metrics({"metrics": {
                    "path": "local",
                    "source_resolution_us": 1,
                    "operation_claim_us": 1,
                    "snapshot_artifact_us": 1,
                    "child_ready_us": 1,
                    "operation_commit_us": 1,
                    "total_us": 5,
                    "live_snapshot": {
                        "rounds": 1,
                        "pages_copied": 1,
                        "final_dirty_pages": 0,
                        "elapsed_us": 10,
                        "downtime_us": 11,
                        "termination": "converged",
                    },
                }}, 2)

    def test_vm_fork_metrics_use_the_source_vcpu_count(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            soak = self.make_soak(Path(directory) / "status.json")
            soak.anchors["source"] = SimpleNamespace(vcpus=4)
            recorded = []
            soak.record_fork_metrics = lambda response, vcpus: recorded.append(
                (response, vcpus)
            )
            response = {"metrics": {"path": "local"}}

            soak.record_vm_fork_metrics("source", response)

            self.assertEqual(recorded, [(response, 4)])


if __name__ == "__main__":
    unittest.main()
