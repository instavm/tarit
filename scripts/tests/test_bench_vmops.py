"""Run both benchmark entry points against a stateful loopback API fixture."""

import json
import os
import subprocess
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from uuid import UUID

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/bench-vmops.sh"
LEGACY_SCRIPT = ROOT / "orch/scripts/bench-vmops.sh"
FOREIGN_VM = "11111111-1111-4111-8111-111111111111"
FOREIGN_SNAPSHOT = "22222222-2222-4222-8222-222222222222"


class BenchmarkContractTests(unittest.TestCase):
    def run_benchmark(self, *, script=SCRIPT, failure=None):
        state = {
            "requests": [],
            "vms": {FOREIGN_VM},
            "created_vms": set(),
            "snapshots": {FOREIGN_SNAPSHOT: FOREIGN_VM},
            "created_snapshots": set(),
            "deleted_vms": set(),
            "deleted_snapshots": set(),
            "restores": [],
            "max_benchmark_vms": 0,
            "protected_vms": {FOREIGN_VM},
        }

        class Handler(BaseHTTPRequestHandler):
            def handle_request(self):
                data = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                body = json.loads(data) if data else None
                state["requests"].append((self.command, self.path, body))
                result = self.dispatch(body)
                if result is None:
                    self.close_connection = True
                    return
                status, response = result
                payload = json.dumps(response).encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def dispatch(self, body):
                if self.command == "DELETE":
                    resource_id = self.path.rsplit("/", 1)[1]
                    if failure == "delete_source" and resource_id == state.get("source"):
                        return 500, {"error": "delete failed"}
                    if self.path.startswith("/v1/vms/"):
                        state["deleted_vms"].add(resource_id)
                        state["vms"].discard(resource_id)
                    else:
                        state["deleted_snapshots"].add(resource_id)
                        state["snapshots"].pop(resource_id, None)
                    return 204, {}
                if self.command == "GET":
                    if self.path == "/v1/snapshots":
                        return 200, [
                            {"snapshot_id": key, "source_vm_id": value} for key, value in state["snapshots"].items()
                        ]
                    if self.path.endswith("/status"):
                        return (
                            (500, {"error": "status failed"}) if failure == "status" else (200, {"status": "running"})
                        )
                    resource_id = self.path.rsplit("/", 1)[1]
                    if failure == "occupied_probe":
                        state["protected_vms"].add(resource_id)
                        state["vms"].add(resource_id)
                    return (200, {"id": resource_id}) if resource_id in state["vms"] else (404, {})
                if self.path in ("/v1/vms", "/v1/restore"):
                    resource_id = str(UUID(body["id"]))
                    if failure == "conflict":
                        state["protected_vms"].add(resource_id)
                        state["vms"].add(resource_id)
                        return 409, {"id": resource_id}
                    state["vms"].add(resource_id)
                    state["created_vms"].add(resource_id)
                    state["max_benchmark_vms"] = max(
                        state["max_benchmark_vms"], len(state["vms"] - state["protected_vms"])
                    )
                    if self.path == "/v1/vms":
                        state["source"] = resource_id
                        if failure == "create_after_publish":
                            return 500, {"id": FOREIGN_VM}
                        if failure == "unexpected_vm_id":
                            return 201, {"id": FOREIGN_VM}
                    else:
                        state["restores"].append(body)
                        if len(state["restores"]) == 2:
                            if failure == "restore_after_publish":
                                return 500, {"id": FOREIGN_VM}
                            if failure == "restore_disconnect":
                                return None
                    return 201, {"id": resource_id}
                if self.path.endswith("/snapshot"):
                    resource_id = str(UUID(int=100 + len(state["created_snapshots"])))
                    state["snapshots"][resource_id] = state["source"]
                    state["created_snapshots"].add(resource_id)
                    if failure == "snapshot_after_publish":
                        return 500, {"snapshot_id": FOREIGN_SNAPSHOT}
                    if failure == "snapshot_disconnect":
                        return None
                    if failure == "missing_snapshot_id":
                        return 200, {}
                    return 200, {"snapshot_id": resource_id}
                return 200, {"status": "running"}

            do_GET = handle_request
            do_POST = handle_request
            do_DELETE = handle_request

            def log_message(self, *_args):
                pass

        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                result = subprocess.run(
                    ["bash", str(script)],
                    env={**os.environ, "API": f"http://127.0.0.1:{server.server_port}", "ITERS": "3"},
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=20,
                )
            finally:
                server.shutdown()
                thread.join()
        self.assertFalse(state["deleted_vms"] & state["protected_vms"])
        self.assertNotIn(FOREIGN_SNAPSHOT, state["deleted_snapshots"])
        return result, state

    def assert_cleaned(self, state):
        self.assertEqual(state["vms"], state["protected_vms"])
        self.assertEqual(state["snapshots"], {FOREIGN_SNAPSHOT: FOREIGN_VM})
        self.assertEqual(state["created_vms"], state["deleted_vms"])
        self.assertEqual(state["created_snapshots"], state["deleted_snapshots"])

    def test_both_entry_points_use_snapshot_ids_and_release_all_resources(self):
        for script in (SCRIPT, LEGACY_SCRIPT):
            with self.subTest(script=script):
                result, state = self.run_benchmark(script=script)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("restore  : p50=", result.stdout)
                self.assertIn("done.", result.stdout)
                self.assertEqual(len(state["restores"]), 3)
                self.assertEqual(len(state["created_snapshots"]), 4)
                self.assertEqual(state["max_benchmark_vms"], 2, "restore VMs must be deleted between iterations")
                for restore in state["restores"]:
                    self.assertEqual(set(restore), {"id", "snapshot_id"})
                    self.assertIn(restore["snapshot_id"], state["created_snapshots"])
                self.assert_cleaned(state)

    def test_failures_clean_up_published_resources_without_adopting_error_ids(self):
        for failure in (
            "status",
            "create_after_publish",
            "snapshot_after_publish",
            "missing_snapshot_id",
            "restore_after_publish",
            "restore_disconnect",
            "snapshot_disconnect",
            "unexpected_vm_id",
        ):
            with self.subTest(failure=failure):
                result, state = self.run_benchmark(failure=failure)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("done.", result.stdout)
                self.assert_cleaned(state)

    def test_occupied_and_conflicting_vm_ids_are_never_deleted(self):
        for failure in ("occupied_probe", "conflict"):
            with self.subTest(failure=failure):
                result, state = self.run_benchmark(failure=failure)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(state["deleted_vms"])
                self.assertFalse(state["deleted_snapshots"])

    def test_cleanup_failure_is_reported_and_other_resources_are_still_deleted(self):
        result, state = self.run_benchmark(failure="delete_source")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("done.", result.stdout)
        self.assertIn(f"/v1/vms/{state['source']}", result.stderr)
        self.assertEqual(state["vms"], {FOREIGN_VM, state["source"]})
        self.assertEqual(state["snapshots"], {FOREIGN_SNAPSHOT: FOREIGN_VM})
