import json
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from uuid import uuid4

import httpx
from tarit_sdk.api.default import create_volume, get_health, list_snapshots
from tarit_sdk.client import Client
from tarit_sdk.errors import UnexpectedStatus
from tarit_sdk.high_level import TaritClient, TaritDeadlineExceeded
from tarit_sdk.models import CreateVolumeRequest, Volume


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

    def test_create_and_decode_nfs_volume(self):
        request = CreateVolumeRequest(name="data", size_bytes=1048576, provider="nfs_v4_1_block")
        record = {
            **request.to_dict(),
            "id": str(uuid4()),
            "storage_class": "block",
            "status": "available",
            "capabilities": {
                "read_only_many": False,
                "read_write_once": True,
                "read_write_many": False,
                "snapshots": False,
                "clones": False,
            },
            "generation": 1,
            "revision": 1,
            "created_at": "2026-11-22T00:00:00Z",
            "updated_at": "2026-11-22T00:00:00Z",
        }

        def handler(http_request):
            self.assertEqual(http_request.url.path, "/v1/volumes")
            self.assertEqual(json.loads(http_request.content), request.to_dict())
            return httpx.Response(201, json=record)

        with httpx.Client(base_url="http://tarit.test", transport=httpx.MockTransport(handler)) as http:
            client = Client(base_url="http://tarit.test").set_httpx_client(http)
            result = create_volume.sync(client=client, body=request)
            self.assertIsInstance(result, Volume)
            self.assertEqual(result.provider, "nfs_v4_1_block")

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
