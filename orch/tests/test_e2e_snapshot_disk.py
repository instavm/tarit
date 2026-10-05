"""Exercise the real snapshot-disk shell harness without a VMM or KVM.

The fake API follows SnapshotResponse/RestoreRequest's opaque-handle contract.
It models disk markers only to check the harness's requests and assertions;
actual snapshot/restore isolation still requires the live KVM gate.
"""

import json
import os
import re
import shlex
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import unittest
import uuid
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

HARNESS = Path(__file__).with_name("e2e_snapshot_disk.sh")
SNAPSHOT_ID = "dd3efb1c-883a-48c6-9065-995580a6b925"


class FakeApi(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, data, status=200):
        body = json.dumps(data, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/health":
            self.reply({"status": "ok"})
        else:
            self.reply({"error": "unexpected endpoint"}, 404)

    def record(self, body=None):
        with self.server.requests.open("a") as log:
            log.write(
                json.dumps({"method": self.command, "path": self.path, "body": body})
                + "\n"
            )

    def create_vm(self, state):
        vm_id = str(uuid.UUID(int=self.server.next_vm))
        self.server.next_vm += 1
        self.server.vms[vm_id] = state
        (self.server.overlays / f"{vm_id}.cow").touch()
        self.reply({"id": vm_id}, 201)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.record(body)
        if self.path == "/v1/vms":
            self.create_vm({"marker": ""})
        elif self.path.endswith("/snapshot"):
            source_id = self.path.split("/")[-2]
            self.server.snapshot = self.server.vms[source_id].copy()
            artifact = self.server.overlays.parent / "snapshot.ram"
            if self.server.mode != "missing-artifact":
                artifact.write_bytes(b"fake snapshot")
            if self.server.mode != "missing-record":
                self.server.db.execute(
                    "INSERT INTO snapshots(path, snapshot_id) VALUES (?, ?)",
                    (str(artifact), SNAPSHOT_ID),
                )
                self.server.db.commit()
            # Public responses must not expose a host path.
            self.reply({"snapshot_id": SNAPSHOT_ID})
        elif self.path == "/v1/restore":
            if body != {"snapshot_id": SNAPSHOT_ID}:
                self.reply(
                    {"error": "restore requires only the opaque snapshot_id"}, 400
                )
                return
            state = self.server.snapshot
            if self.server.mode != "shared-restores":
                state = state.copy()
            self.create_vm(state)
        elif self.path == "/v1/execute":
            state = self.server.vms[body["vm_id"]]
            marker = re.search(r"echo ([a-z-]+) >", body["command"])
            if marker:
                state["marker"] = marker.group(1)
                stdout = ""
            elif body["command"] == "cat /root/tarit-snapshot-state":
                stdout = state["marker"] + "\n"
            else:
                self.reply({"error": "unexpected guest command"}, 400)
                return
            self.reply({"exit_code": 0, "stdout": stdout, "stderr": ""})
        else:
            self.reply({"error": "unexpected endpoint"}, 404)

    def do_DELETE(self):
        self.record()
        vm_id = self.path.split("/")[-1]
        del self.server.vms[vm_id]
        (self.server.overlays / f"{vm_id}.cow").unlink()
        self.reply({})


def fake_taritd():
    host, port = os.environ["TARIT_LISTEN"].rsplit(":", 1)
    Path(os.environ["FAKE_TARITD_PID"]).write_text(str(os.getpid()))
    with HTTPServer((host, int(port)), FakeApi) as server:
        server.mode = os.environ["FAKE_SNAPSHOT_MODE"]
        server.requests = Path(os.environ["FAKE_SNAPSHOT_REQUESTS"])
        server.overlays = Path(os.environ["TARIT_SOCKET_DIR"]) / "overlays"
        server.overlays.mkdir()
        server.db = sqlite3.connect(os.environ["TARIT_DB"])
        server.db.execute(
            "CREATE TABLE snapshots(path TEXT PRIMARY KEY, snapshot_id TEXT UNIQUE)"
        )
        server.db.commit()
        server.vms = {}
        server.next_vm = 1
        server.serve_forever()


class SnapshotDiskContractTests(unittest.TestCase):
    def run_harness(self, mode="normal"):
        with tempfile.TemporaryDirectory(
            prefix="tarit snapshot contract "
        ) as temporary:
            root = Path(temporary)
            commands = root / "bin"
            commands.mkdir()
            fake = commands / "taritd"
            fake.write_text(
                "#!/bin/sh\nexec "
                + shlex.join(
                    [sys.executable, str(Path(__file__).resolve()), "--fake-taritd"]
                )
                + ' "$@"\n'
            )
            fake.chmod(0o755)
            if shutil.which("setsid") is None:
                # macOS lacks the launcher; retain its process-group behavior.
                launcher = commands / "setsid"
                launcher.write_text(
                    "#!/usr/bin/env python3\nimport os, sys\n"
                    "os.setsid()\nos.execvp(sys.argv[1], sys.argv[1:])\n"
                )
                launcher.chmod(0o755)
            requests = root / "requests.jsonl"
            pid_file = root / "taritd.pid"
            env = {
                **os.environ,
                "PATH": str(commands) + os.pathsep + os.environ["PATH"],
                "TMPDIR": str(root),
                "TARITD_BIN": str(fake),
                "SNAPSHOT_DISK_E2E_PORT": "",
                "FAKE_TARITD_PID": str(pid_file),
                "FAKE_SNAPSHOT_MODE": mode,
                "FAKE_SNAPSHOT_REQUESTS": str(requests),
            }
            try:
                result = subprocess.run(
                    ["bash", str(HARNESS)],
                    env=env,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    timeout=20,
                )
            finally:
                # Also stop the fixture if a broken harness times out.
                if pid_file.exists():
                    try:
                        os.killpg(int(pid_file.read_text()), signal.SIGTERM)
                    except ProcessLookupError:
                        pass
            self.assertFalse(
                list(root.glob("tarit-snapshot-disk-e2e.*")), "harness cleanup failed"
            )
            self.assertTrue(requests.exists(), result.stdout)
            calls = [json.loads(line) for line in requests.read_text().splitlines()]
            return result, calls

    def test_opaque_handle_response_and_restore_requests(self):
        result, calls = self.run_harness()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("RESULT: SNAPSHOT_DISK_PASS", result.stdout)
        restores = [call["body"] for call in calls if call["path"] == "/v1/restore"]
        self.assertEqual(restores, [{"snapshot_id": SNAPSHOT_ID}] * 2)
        self.assertEqual(sum(call["method"] == "DELETE" for call in calls), 3)

    def test_missing_snapshot_record_fails_before_restore(self):
        result, calls = self.run_harness("missing-record")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("snapshot is missing from the local test database", result.stdout)
        self.assertFalse(any(call["path"] == "/v1/restore" for call in calls))

    def test_missing_snapshot_artifact_fails_before_restore(self):
        result, calls = self.run_harness("missing-artifact")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(call["path"] == "/v1/restore" for call in calls))
        self.assertNotIn("RESULT: SNAPSHOT_DISK_PASS", result.stdout)

    def test_shared_restore_state_is_rejected(self):
        result, calls = self.run_harness("shared-restores")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(sum(call["path"] == "/v1/restore" for call in calls), 2)
        self.assertNotIn("RESULT: SNAPSHOT_DISK_PASS", result.stdout)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--fake-taritd"]:
        fake_taritd()
    else:
        unittest.main()
