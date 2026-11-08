"""Exercise the real benchmark against the public snapshot/restore response shape."""

import json
import os
import subprocess
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

VM_ID = "11111111-1111-4111-8111-111111111111"
SNAPSHOT_ID = "22222222-2222-4222-8222-222222222222"
SCRIPT = Path(__file__).resolve().parents[1] / "bench-vmops.sh"


class BenchmarkContractTests(unittest.TestCase):
    def run_benchmark(self, failure_path=None, missing_snapshot_id=False):
        requests = []

        class Handler(BaseHTTPRequestHandler):
            def handle_request(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                requests.append(
                    (self.command, self.path, json.loads(body) if body else None)
                )
                if self.path == failure_path:
                    status, response = 500, {"error": "operation failed"}
                elif self.path == "/v1/vms":
                    status, response = 201, {"id": VM_ID}
                elif self.path.endswith("/snapshot"):
                    status, response = (
                        200,
                        {} if missing_snapshot_id else {"snapshot_id": SNAPSHOT_ID},
                    )
                else:
                    status, response = 200, {"status": "running"}
                payload = json.dumps(response).encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            do_GET = handle_request
            do_POST = handle_request

            def log_message(self, *_args):
                pass

        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                result = subprocess.run(
                    ["bash", str(SCRIPT)],
                    env={
                        **os.environ,
                        "API": f"http://127.0.0.1:{server.server_port}",
                        "ITERS": "1",
                    },
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
            finally:
                server.shutdown()
                thread.join()
        return result, requests

    def test_restore_uses_opaque_snapshot_id(self):
        result, requests = self.run_benchmark()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("restore  : p50=", result.stdout)
        self.assertIn("done.", result.stdout)
        restores = [
            body
            for method, path, body in requests
            if method == "POST" and path == "/v1/restore"
        ]
        self.assertEqual(restores, [{"snapshot_id": SNAPSHOT_ID}])

    def test_http_errors_fail_the_benchmark(self):
        for path in (
            f"/v1/vms/{VM_ID}/status",
            f"/v1/vms/{VM_ID}/snapshot",
            "/v1/restore",
        ):
            with self.subTest(path=path):
                result, _ = self.run_benchmark(failure_path=path)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("done.", result.stdout)

    def test_missing_snapshot_id_fails_instead_of_skipping_restore(self):
        result, requests = self.run_benchmark(missing_snapshot_id=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("done.", result.stdout)
        self.assertFalse(any(path == "/v1/restore" for _, path, _ in requests))
