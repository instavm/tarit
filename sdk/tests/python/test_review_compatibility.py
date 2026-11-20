import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from uuid import uuid4

import httpx

from tarit_sdk.api.default import get_health, list_snapshots
from tarit_sdk.client import Client
from tarit_sdk.errors import UnexpectedStatus
from tarit_sdk.high_level import TaritClient, TaritDeadlineExceeded


class CompatibilityTests(unittest.TestCase):
    def test_public_methods_with_legacy_health_and_snapshot_outage(self):
        def handler(request):
            if request.url.path == "/health":
                return httpx.Response(200, json={"status": "ok"})
            return httpx.Response(503, json={"error": "fleet unavailable"})

        with httpx.Client(base_url="http://tarit.test", transport=httpx.MockTransport(handler)) as http:
            client = Client(base_url="http://tarit.test", raise_on_unexpected_status=True).set_httpx_client(http)
            self.assertEqual(get_health.sync_detailed(client=client).parsed.status, "ok")
            with self.assertRaises(UnexpectedStatus) as raised:
                list_snapshots.sync(client=client)
            self.assertEqual(raised.exception.status_code, 503)

    def test_slow_response_body_cannot_extend_sync_cleanup_deadline(self):
        finished = threading.Event()

        class Handler(BaseHTTPRequestHandler):
            def do_DELETE(self):
                self.send_response(500)
                self.send_header("Content-Length", "100")
                self.end_headers()
                try:
                    for _ in range(100):
                        self.wfile.write(b"x")
                        if finished.wait(0.02):
                            return
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def log_message(self, *args):
                pass

        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with TaritClient(f"http://127.0.0.1:{server.server_port}", "key") as client:
                    client.raw.get_httpx_client()
                    start = time.monotonic()
                    with self.assertRaises(TaritDeadlineExceeded):
                        client.delete_pty_session(uuid4(), uuid4(), deadline_seconds=0.15)
                    self.assertLess(time.monotonic() - start, 0.8)
            finally:
                finished.set()
                server.shutdown()
                thread.join()
