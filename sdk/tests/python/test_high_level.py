from __future__ import annotations

import json
import unittest
from uuid import UUID

import httpx
from tarit_sdk.high_level import (
    AsyncTaritClient,
    TaritApiError,
    TaritClient,
    TaritDeadlineExceeded,
)

VM_ID = UUID("11111111-1111-4111-8111-111111111111")
CHILD_ID = UUID("22222222-2222-4222-8222-222222222222")
EXECUTION_ID = UUID("33333333-3333-4333-8333-333333333333")
NOW = "2026-08-31T00:00:00Z"


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
