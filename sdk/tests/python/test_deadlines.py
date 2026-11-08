from __future__ import annotations

import asyncio
import unittest
from types import SimpleNamespace
from unittest.mock import patch

import httpx
from tarit_sdk import high_level
from tarit_sdk.high_level import AsyncTaritClient, TaritClient, TaritDeadlineExceeded
from test_high_level import CHILD_ID, EXECUTION_ID, VM_ID, execution, fork_result


class Clock:
    def __init__(self) -> None:
        self.now = 100.0

    def advance(self, seconds: float) -> None:
        self.now += seconds

    def patched(self):
        return patch(
            "tarit_sdk.high_level.time",
            SimpleNamespace(monotonic=lambda: self.now, sleep=self.advance),
        )


class SyncDeadlineTests(unittest.TestCase):
    def test_response_decoding_counts_toward_deadline(self) -> None:
        clock = Clock()
        parse = high_level._parsed

        def slow_parse(*args):
            result = parse(*args)
            clock.advance(2)
            return result

        def handler(request: httpx.Request) -> httpx.Response:
            if request.url.path.endswith("/fork"):
                return httpx.Response(201, json=fork_result())
            return httpx.Response(200, json=execution("completed"))

        with (
            clock.patched(),
            patch("tarit_sdk.high_level._parsed", side_effect=slow_parse),
            TaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client,
        ):
            with self.assertRaises(TaritDeadlineExceeded):
                client.wait_execution(EXECUTION_ID, deadline_seconds=1)
            with self.assertRaises(TaritDeadlineExceeded):
                client.fork(VM_ID, deadline_seconds=1)

    def test_submission_and_poll_share_one_budget(self) -> None:
        clock = Clock()
        budgets = []

        def handler(request: httpx.Request) -> httpx.Response:
            budgets.append(request.extensions["timeout"]["read"])
            if request.method == "POST":
                clock.advance(0.75)
                return httpx.Response(202, json=execution("pending"))
            return httpx.Response(200, json=execution("completed"))

        with (
            clock.patched(),
            TaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client,
        ):
            client.execute(VM_ID, "echo ok", deadline_seconds=1)
            self.assertEqual(client.raw.get_httpx_client().timeout.read, 30)
        self.assertEqual(budgets, [1, 0.25])

    def test_late_responses_never_count_as_success(self) -> None:
        for operation in ("execute", "wait_execution", "fork"):
            with self.subTest(operation=operation):
                clock = Clock()
                requests = []

                def handler(
                    request: httpx.Request, clock=clock, operation=operation, requests=requests
                ) -> httpx.Response:
                    requests.append(request)
                    clock.advance(2)
                    if operation == "fork":
                        return httpx.Response(201, json=fork_result())
                    return httpx.Response(
                        202 if operation == "execute" else 200,
                        json=execution("completed"),
                    )

                with (
                    clock.patched(),
                    TaritClient(
                        "https://tarit.test",
                        "key",
                        transport=httpx.MockTransport(handler),
                    ) as client,
                    self.assertRaises(TaritDeadlineExceeded),
                ):
                    if operation == "execute":
                        client.execute(VM_ID, "echo ok", deadline_seconds=1)
                    elif operation == "fork":
                        client.fork(VM_ID, child_id=CHILD_ID, deadline_seconds=1)
                    else:
                        client.wait_execution(EXECUTION_ID, deadline_seconds=1)
                self.assertEqual(len(requests), 1)

    def test_expired_poll_and_retry_do_not_start_another_request(self) -> None:
        for operation in ("wait_execution", "fork"):
            with self.subTest(operation=operation):
                clock = Clock()
                calls = []

                def handler(request: httpx.Request, operation=operation, calls=calls) -> httpx.Response:
                    calls.append(request)
                    return (
                        httpx.Response(503, json={})
                        if operation == "fork"
                        else httpx.Response(200, json=execution("running"))
                    )

                with (
                    clock.patched(),
                    TaritClient(
                        "https://tarit.test",
                        "key",
                        transport=httpx.MockTransport(handler),
                    ) as client,
                    self.assertRaises(TaritDeadlineExceeded),
                ):
                    if operation == "fork":
                        client.fork(VM_ID, deadline_seconds=0.05)
                    else:
                        client.wait_execution(EXECUTION_ID, deadline_seconds=0.05, poll_interval=0.1)
                self.assertEqual(len(calls), 1)

    def test_request_timeout_still_caps_shorter_network_operations(self) -> None:
        def handler(request: httpx.Request) -> httpx.Response:
            self.assertTrue(all(value == 0.25 for value in request.extensions["timeout"].values()))
            return httpx.Response(200, json=execution("completed"))

        with TaritClient(
            "https://tarit.test",
            "key",
            request_timeout=0.25,
            transport=httpx.MockTransport(handler),
        ) as client:
            client.wait_execution(EXECUTION_ID, deadline_seconds=1)
            self.assertEqual(client.raw.get_httpx_client().timeout.read, 0.25)

    def test_invalid_options_are_rejected_before_submission(self) -> None:
        def handler(_request: httpx.Request) -> httpx.Response:
            self.fail("invalid options must not submit work")

        with TaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
            for value in (0, -1, float("inf"), float("nan")):
                with self.subTest(deadline=value), self.assertRaises(ValueError):
                    client.execute(VM_ID, "echo ok", deadline_seconds=value)
            for value in (-1, float("inf"), float("nan")):
                with self.subTest(poll_interval=value), self.assertRaises(ValueError):
                    client.execute(VM_ID, "echo ok", poll_interval=value)


class AsyncDeadlineTests(unittest.IsolatedAsyncioTestCase):
    async def test_response_decoding_counts_toward_deadline(self) -> None:
        clock = Clock()
        parse = high_level._parsed

        def slow_parse(*args):
            result = parse(*args)
            clock.advance(2)
            return result

        async def handler(request: httpx.Request) -> httpx.Response:
            if request.url.path.endswith("/fork"):
                return httpx.Response(201, json=fork_result())
            return httpx.Response(200, json=execution("completed"))

        with clock.patched(), patch("tarit_sdk.high_level._parsed", side_effect=slow_parse):
            async with AsyncTaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
                with self.assertRaises(TaritDeadlineExceeded):
                    await client.wait_execution(EXECUTION_ID, deadline_seconds=1)
                with self.assertRaises(TaritDeadlineExceeded):
                    await client.fork(VM_ID, deadline_seconds=1)

    async def test_submission_and_poll_share_one_budget(self) -> None:
        clock = Clock()
        budgets = []

        async def handler(request: httpx.Request) -> httpx.Response:
            budgets.append(request.extensions["timeout"]["read"])
            if request.method == "POST":
                clock.advance(0.75)
                return httpx.Response(202, json=execution("pending"))
            return httpx.Response(200, json=execution("completed"))

        with clock.patched():
            async with AsyncTaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
                await client.execute(VM_ID, "echo ok", deadline_seconds=1)
                self.assertEqual(client.raw.get_async_httpx_client().timeout.read, 30)
        self.assertEqual(budgets, [1, 0.25])

    async def test_late_responses_never_count_as_success(self) -> None:
        for operation in ("execute", "wait_execution", "fork"):
            with self.subTest(operation=operation):
                clock = Clock()
                requests = []

                async def handler(
                    request: httpx.Request, clock=clock, operation=operation, requests=requests
                ) -> httpx.Response:
                    requests.append(request)
                    clock.advance(2)
                    if operation == "fork":
                        return httpx.Response(201, json=fork_result())
                    return httpx.Response(
                        202 if operation == "execute" else 200,
                        json=execution("completed"),
                    )

                with clock.patched():
                    async with AsyncTaritClient(
                        "https://tarit.test",
                        "key",
                        transport=httpx.MockTransport(handler),
                    ) as client:
                        with self.assertRaises(TaritDeadlineExceeded):
                            if operation == "execute":
                                await client.execute(VM_ID, "echo ok", deadline_seconds=1)
                            elif operation == "fork":
                                await client.fork(VM_ID, child_id=CHILD_ID, deadline_seconds=1)
                            else:
                                await client.wait_execution(EXECUTION_ID, deadline_seconds=1)
                self.assertEqual(len(requests), 1)

    async def test_deadline_cancels_an_inflight_request(self) -> None:
        for operation in ("execute", "wait_execution", "fork"):
            with self.subTest(operation=operation):
                cancelled = False

                async def handler(_request: httpx.Request) -> httpx.Response:
                    nonlocal cancelled
                    try:
                        await asyncio.sleep(60)
                        self.fail("request was not cancelled")
                    except asyncio.CancelledError:
                        cancelled = True
                        raise

                async with AsyncTaritClient(
                    "https://tarit.test", "key", transport=httpx.MockTransport(handler)
                ) as client:
                    with self.assertRaises(TaritDeadlineExceeded):
                        if operation == "execute":
                            await client.execute(VM_ID, "echo ok", deadline_seconds=0.01)
                        elif operation == "fork":
                            await client.fork(VM_ID, deadline_seconds=0.01)
                        else:
                            await client.wait_execution(EXECUTION_ID, deadline_seconds=0.01)
                self.assertTrue(cancelled)

    async def test_invalid_options_are_rejected_before_submission(self) -> None:
        async def handler(_request: httpx.Request) -> httpx.Response:
            self.fail("invalid options must not submit work")

        async with AsyncTaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
            for value in (0, -1, float("inf"), float("nan")):
                with self.subTest(deadline=value), self.assertRaises(ValueError):
                    await client.execute(VM_ID, "echo ok", deadline_seconds=value)
            for value in (-1, float("inf"), float("nan")):
                with self.subTest(poll_interval=value), self.assertRaises(ValueError):
                    await client.execute(VM_ID, "echo ok", poll_interval=value)
