from __future__ import annotations

import asyncio
import json
import unittest
from unittest.mock import AsyncMock, patch
from uuid import UUID

import httpx
from tarit_sdk.high_level import (
    AsyncTaritClient,
    PtyData,
    PtyExit,
    TaritApiError,
    TaritClient,
    TaritDeadlineExceeded,
    TaritPtyConnectionError,
)

VM_ID = UUID("11111111-1111-4111-8111-111111111111")
CHILD_ID = UUID("22222222-2222-4222-8222-222222222222")
EXECUTION_ID = UUID("33333333-3333-4333-8333-333333333333")
NOW = "2026-08-31T00:00:00Z"
PTY_ID = UUID("44444444-4444-4444-8444-444444444444")


def execution(status: str, **fields: object) -> dict[str, object]:
    return {
        "id": str(EXECUTION_ID),
        "vm_id": str(VM_ID),
        "command": "echo sdk-ok",
        "timeout_ms": 30000,
        "status": status,
        "created_at": NOW,
        "updated_at": NOW,
        **fields,
    }


def fork_result() -> dict[str, object]:
    return {
        "source_vm_id": str(VM_ID),
        "vm": {
            "id": str(CHILD_ID),
            "status": "running",
            "revision": 1,
            "memory_mib": 256,
            "vcpus": 1,
            "created_at": NOW,
            "updated_at": NOW,
        },
    }


class FakeSyncWebSocket:
    def __init__(self, messages: list[str | bytes]) -> None:
        self.messages = messages
        self.sent: list[str | bytes] = []
        self.closed = False

    def send(self, data: str | bytes) -> None:
        self.sent.append(data)

    def recv(self, timeout: float | None = None) -> str | bytes:
        del timeout
        if not self.messages:
            raise TimeoutError
        return self.messages.pop(0)

    def close(self) -> None:
        self.closed = True


class FakeAsyncWebSocket:
    def __init__(self, messages: list[str | bytes]) -> None:
        self.messages = messages
        self.sent: list[str | bytes] = []
        self.closed = False

    async def send(self, data: str | bytes) -> None:
        self.sent.append(data)

    async def recv(self) -> str | bytes:
        if not self.messages:
            await asyncio.sleep(3600)
        return self.messages.pop(0)

    async def close(self) -> None:
        self.closed = True


class SyncClientTests(unittest.TestCase):
    def test_execute_polls_to_terminal_and_uses_api_key(self) -> None:
        polls = 0

        def handler(request: httpx.Request) -> httpx.Response:
            nonlocal polls
            self.assertEqual(request.headers["X-API-Key"], "tenant-key")
            if request.url.path == "/v1/execute_async":
                return httpx.Response(202, json=execution("pending"))
            self.assertEqual(request.url.path, f"/v1/executions/{EXECUTION_ID}")
            polls += 1
            if polls == 1:
                return httpx.Response(200, json=execution("running"))
            return httpx.Response(200, json=execution("completed", exit_code=0, stdout="sdk-ok\n", stderr=""))

        with TaritClient("https://tarit.test/", "tenant-key", transport=httpx.MockTransport(handler)) as client:
            result = client.execute(VM_ID, "echo sdk-ok", poll_interval=0)
        self.assertEqual(result.status, "completed")
        self.assertEqual(result.stdout, "sdk-ok\n")
        self.assertEqual(polls, 2)

    def test_fork_retries_with_one_stable_child_id(self) -> None:
        bodies: list[dict[str, object]] = []

        def handler(request: httpx.Request) -> httpx.Response:
            bodies.append(json.loads(request.content))
            if len(bodies) == 1:
                return httpx.Response(503, json={"error": "target temporarily unavailable"})
            return httpx.Response(201, json=fork_result())

        with TaritClient("https://tarit.test", "tenant-key", transport=httpx.MockTransport(handler)) as client:
            result = client.fork(VM_ID, child_id=CHILD_ID, deadline_seconds=1)
        self.assertEqual(result.vm.id, CHILD_ID)
        self.assertEqual(bodies, [{"id": str(CHILD_ID)}, {"id": str(CHILD_ID)}])

    def test_tenant_denial_is_typed(self) -> None:
        def handler(_request: httpx.Request) -> httpx.Response:
            return httpx.Response(403, json={"error": "VM belongs to another tenant"})

        with (
            TaritClient("https://tarit.test", "tenant-key", transport=httpx.MockTransport(handler)) as client,
            self.assertRaisesRegex(TaritApiError, "another tenant") as raised,
        ):
            client.wait_execution(EXECUTION_ID, poll_interval=0)
        self.assertEqual(raised.exception.status_code, 403)

    def test_fork_network_failure_obeys_deadline(self) -> None:
        def handler(request: httpx.Request) -> httpx.Response:
            raise httpx.ConnectError("unreachable", request=request)

        with (
            TaritClient("https://tarit.test", "tenant-key", transport=httpx.MockTransport(handler)) as client,
            self.assertRaises(TaritDeadlineExceeded),
        ):
            client.fork(VM_ID, child_id=CHILD_ID, deadline_seconds=0)

    def test_open_pty_bridges_binary_resize_exit_and_deletes_session(self) -> None:
        requests: list[httpx.Request] = []

        def handler(request: httpx.Request) -> httpx.Response:
            requests.append(request)
            self.assertEqual(request.headers["X-API-Key"], "tenant-key")
            if request.method == "POST":
                self.assertEqual(
                    json.loads(request.content),
                    {"cols": 100, "rows": 40, "shell": "/bin/sh"},
                )
                return httpx.Response(
                    201,
                    json={"pty_id": str(PTY_ID), "cols": 100, "rows": 40, "connect_token": "pty-secret"},
                )
            return httpx.Response(204)

        websocket = FakeSyncWebSocket([b"prompt", '{"type":"exit","exit_code":7}'])
        with (
            patch("tarit_sdk.high_level.sync_websocket_connect", return_value=websocket) as connect,
            TaritClient("https://tarit.test/base", "tenant-key", transport=httpx.MockTransport(handler)) as client,
            client.open_pty(VM_ID, cols=100, rows=40, shell="/bin/sh") as pty,
        ):
            self.assertEqual(pty.pty_id, PTY_ID)
            pty.write("echo sdk-pty\n")
            pty.resize(120, 50)
            self.assertEqual(pty.read(), PtyData(b"prompt"))
            self.assertEqual(pty.read(), PtyExit(7))

        url = connect.call_args.args[0]
        self.assertEqual(
            url,
            f"wss://tarit.test/base/v1/vms/{VM_ID}/pty/{PTY_ID}/connect?token=pty-secret",
        )
        self.assertNotIn("tenant-key", url)
        self.assertEqual(
            websocket.sent,
            [b"echo sdk-pty\n", '{"type":"resize","cols":120,"rows":50}'],
        )
        self.assertTrue(websocket.closed)
        self.assertEqual(
            [(request.method, request.url.path) for request in requests],
            [
                ("POST", f"/base/v1/vms/{VM_ID}/pty/sessions"),
                ("DELETE", f"/base/v1/vms/{VM_ID}/pty/sessions/{PTY_ID}"),
            ],
        )

    def test_open_pty_connection_failure_deletes_lease_without_leaking_token(self) -> None:
        methods: list[str] = []

        def handler(request: httpx.Request) -> httpx.Response:
            methods.append(request.method)
            if request.method == "POST":
                return httpx.Response(
                    201,
                    json={"pty_id": str(PTY_ID), "cols": 80, "rows": 24, "connect_token": "pty-secret"},
                )
            return httpx.Response(204)

        with (
            patch("tarit_sdk.high_level.sync_websocket_connect", side_effect=OSError("dial failed")),
            TaritClient("https://tarit.test", "tenant-key", transport=httpx.MockTransport(handler)) as client,
            self.assertRaises(TaritPtyConnectionError) as raised,
        ):
            client.open_pty(VM_ID)
        self.assertNotIn("pty-secret", str(raised.exception))
        self.assertEqual(methods, ["POST", "DELETE"])

    def test_open_pty_bounds_session_creation(self) -> None:
        with TaritClient("https://tarit.test", "tenant-key") as client:
            http = client.raw.get_httpx_client()
            with (
                patch.object(http, "post", side_effect=httpx.ReadTimeout("slow create")) as post,
                self.assertRaises(TaritDeadlineExceeded),
            ):
                client.open_pty(VM_ID, deadline_seconds=0.25)
        self.assertEqual(post.call_args.kwargs["timeout"], 0.25)


class AsyncClientTests(unittest.IsolatedAsyncioTestCase):
    async def test_execute_polls_to_terminal(self) -> None:
        polls = 0

        async def handler(request: httpx.Request) -> httpx.Response:
            nonlocal polls
            self.assertEqual(request.headers["X-API-Key"], "tenant-key")
            if request.url.path == "/v1/execute_async":
                return httpx.Response(202, json=execution("pending"))
            polls += 1
            return httpx.Response(200, json=execution("completed", exit_code=0, stdout="sdk-ok\n"))

        transport = httpx.MockTransport(handler)
        async with AsyncTaritClient("https://tarit.test", "tenant-key", transport=transport) as client:
            result = await client.execute(VM_ID, "echo sdk-ok", poll_interval=0)
        self.assertEqual(result.status, "completed")
        self.assertEqual(polls, 1)

    async def test_open_pty_async_bridges_frames_and_cleans_up(self) -> None:
        methods: list[str] = []

        async def handler(request: httpx.Request) -> httpx.Response:
            methods.append(request.method)
            if request.method == "POST":
                return httpx.Response(
                    201,
                    json={"pty_id": str(PTY_ID), "cols": 80, "rows": 24, "connect_token": "async-secret"},
                )
            return httpx.Response(204)

        websocket = FakeAsyncWebSocket([b"async-data", '{"type":"exit","exit_code":0}'])
        with patch(
            "tarit_sdk.high_level.async_websocket_connect",
            new=AsyncMock(return_value=websocket),
        ):
            async with AsyncTaritClient(
                "http://tarit.test", "tenant-key", transport=httpx.MockTransport(handler)
            ) as client:
                async with await client.open_pty(VM_ID) as pty:
                    await pty.write("exit\n")
                    self.assertEqual(await pty.read(), PtyData(b"async-data"))
                    self.assertEqual(await pty.read(), PtyExit(0))
        self.assertEqual(websocket.sent, [b"exit\n"])
        self.assertTrue(websocket.closed)
        self.assertEqual(methods, ["POST", "DELETE"])
