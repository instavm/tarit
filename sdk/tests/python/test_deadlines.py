from __future__ import annotations

import asyncio
import json
import unittest
from types import SimpleNamespace
from unittest.mock import patch
from uuid import UUID

import httpx
from tarit_sdk import high_level
from tarit_sdk.high_level import (
    AsyncTaritClient,
    TaritApiError,
    TaritClient,
    TaritDeadlineExceeded,
)
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
    def test_transient_poll_responses_retry_without_submission_replay(self) -> None:
        clock = Clock()
        statuses = iter([202, 429, 502, 503, 504, 200])
        methods: list[str] = []

        def handler(request: httpx.Request) -> httpx.Response:
            methods.append(request.method)
            status = next(statuses)
            return httpx.Response(status, json=execution("completed" if status == 200 else "pending")
                                  if status in {200, 202} else {"error": "busy"})

        with clock.patched(), TaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
            self.assertEqual(client.execute(VM_ID, "once", deadline_seconds=5).status, "completed")
        self.assertEqual(methods, ["POST", "GET", "GET", "GET", "GET", "GET"])

    def test_persistent_poll_overload_preserves_execution_deadline(self) -> None:
        clock = Clock()
        methods: list[str] = []

        def handler(request: httpx.Request) -> httpx.Response:
            methods.append(request.method)
            return httpx.Response(503, json={"error": "busy"})

        with (
            clock.patched(),
            TaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client,
            self.assertRaises(TaritDeadlineExceeded) as raised,
        ):
            client.wait_execution(EXECUTION_ID, deadline_seconds=.05)
        self.assertEqual(raised.exception.execution_id, EXECUTION_ID)
        self.assertEqual(methods, ["GET"])

    def test_execute_deadline_retains_id_from_submission_through_polling(self) -> None:
        for stage in (
            "submission",
            "submission_decode",
            "poll",
            "poll_decode",
            "poll_timeout",
        ):
            with self.subTest(stage=stage):
                clock = Clock()
                methods = []
                parse = high_level._parsed

                def slow_parse(*args, parse=parse, stage=stage, methods=methods, clock=clock):
                    result = parse(*args)
                    if stage == f"{'submission' if len(methods) == 1 else 'poll'}_decode":
                        clock.advance(2)
                    return result

                def handler(request: httpx.Request, methods=methods, stage=stage, clock=clock) -> httpx.Response:
                    methods.append(request.method)
                    if stage == "submission" or (request.method == "GET" and stage in ("poll", "poll_timeout")):
                        clock.advance(2)
                    if stage == "poll_timeout" and request.method == "GET":
                        raise httpx.ReadTimeout("poll response lost", request=request)
                    return httpx.Response(
                        202 if request.method == "POST" else 200,
                        json=execution("pending" if request.method == "POST" else "completed"),
                    )

                with (
                    clock.patched(),
                    patch("tarit_sdk.high_level._parsed", side_effect=slow_parse),
                    TaritClient(
                        "https://tarit.test",
                        "key",
                        transport=httpx.MockTransport(handler),
                    ) as client,
                    self.assertRaises(TaritDeadlineExceeded) as raised,
                ):
                    client.execute(VM_ID, "echo once", deadline_seconds=1)
                self.assertEqual(raised.exception.execution_id, EXECUTION_ID)
                self.assertIn(str(EXECUTION_ID), str(raised.exception))
                self.assertIsNone(raised.exception.child_id)
                self.assertEqual(
                    methods,
                    ["POST"] if stage.startswith("submission") else ["POST", "GET"],
                )

    def test_submission_timeout_without_response_does_not_invent_execution_id(
        self,
    ) -> None:
        clock = Clock()
        calls = []

        def handler(request: httpx.Request) -> httpx.Response:
            calls.append(request)
            clock.advance(2)
            raise httpx.ReadTimeout("submission response lost", request=request)

        with (
            clock.patched(),
            TaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client,
            self.assertRaises(TaritDeadlineExceeded) as raised,
        ):
            client.execute(VM_ID, "echo once", deadline_seconds=1)
        self.assertIsNone(raised.exception.execution_id)
        self.assertIn("execution ID unavailable", str(raised.exception))
        self.assertIn("may have been accepted", str(raised.exception))
        self.assertEqual(len(calls), 1)

    def test_execution_context_does_not_replace_transport_or_api_errors(self) -> None:
        failure = httpx.ReadTimeout("short network timeout")
        for operation in ("execute", "wait_execution"):
            for response in (failure, httpx.Response(403, json={"error": "forbidden"})):
                with self.subTest(operation=operation, response=response):

                    def handler(_request: httpx.Request, response=response) -> httpx.Response:
                        if isinstance(response, Exception):
                            raise response
                        return response

                    with TaritClient(
                        "https://tarit.test",
                        "key",
                        transport=httpx.MockTransport(handler),
                    ) as client:
                        with self.assertRaises(httpx.ReadTimeout if response is failure else TaritApiError) as raised:
                            if operation == "execute":
                                client.execute(VM_ID, "echo once")
                            else:
                                client.wait_execution(EXECUTION_ID)
                        if response is failure:
                            self.assertIs(raised.exception, failure)

    def test_fork_deadline_exposes_generated_id_after_late_success(self) -> None:
        clock = Clock()
        ids = []

        def handler(request: httpx.Request) -> httpx.Response:
            child_id = json.loads(request.content)["id"]
            ids.append(child_id)
            clock.advance(2)
            result = fork_result()
            result["vm"]["id"] = child_id
            return httpx.Response(201, json=result)

        with (
            clock.patched(),
            TaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client,
            self.assertRaises(TaritDeadlineExceeded) as raised,
        ):
            client.fork(VM_ID, deadline_seconds=1)
        self.assertEqual(raised.exception.child_id, UUID(ids[0]))
        self.assertIn(ids[0], str(raised.exception))
        self.assertEqual(len(ids), 1)

    def test_fork_transport_retry_deadline_retains_the_same_id(self) -> None:
        for supplied in (None, CHILD_ID):
            with self.subTest(child_id=supplied):
                clock = Clock()
                ids = []

                def handler(request: httpx.Request, clock=clock, ids=ids) -> httpx.Response:
                    ids.append(json.loads(request.content)["id"])
                    clock.advance(0.1 if len(ids) == 1 else 2)
                    raise httpx.ReadTimeout("response lost", request=request)

                with (
                    clock.patched(),
                    TaritClient(
                        "https://tarit.test",
                        "key",
                        transport=httpx.MockTransport(handler),
                    ) as client,
                    self.assertRaises(TaritDeadlineExceeded) as raised,
                ):
                    client.fork(VM_ID, child_id=supplied, deadline_seconds=1)
                self.assertEqual(len(ids), 2)
                self.assertEqual(ids, [str(raised.exception.child_id)] * 2)
                if supplied is not None:
                    self.assertEqual(raised.exception.child_id, supplied)

    def test_invalid_fork_deadline_does_not_generate_or_submit_a_child(self) -> None:
        def handler(_request: httpx.Request) -> httpx.Response:
            self.fail("invalid deadline must not submit a fork")

        with (
            patch("tarit_sdk.high_level.uuid4") as new_id,
            TaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client,
        ):
            for value in (0, -1, float("inf"), float("nan")):
                with self.subTest(deadline=value), self.assertRaises(ValueError):
                    client.fork(VM_ID, deadline_seconds=value)
            new_id.assert_not_called()

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
                    request: httpx.Request,
                    clock=clock,
                    operation=operation,
                    requests=requests,
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
                    self.assertRaises(TaritDeadlineExceeded) as raised,
                ):
                    if operation == "execute":
                        client.execute(VM_ID, "echo ok", deadline_seconds=1)
                    elif operation == "fork":
                        client.fork(VM_ID, child_id=CHILD_ID, deadline_seconds=1)
                    else:
                        client.wait_execution(EXECUTION_ID, deadline_seconds=1)
                self.assertEqual(len(requests), 1)
                self.assertEqual(
                    raised.exception.execution_id,
                    None if operation == "fork" else EXECUTION_ID,
                )

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
                    self.assertRaises(TaritDeadlineExceeded) as raised,
                ):
                    if operation == "fork":
                        client.fork(VM_ID, deadline_seconds=0.05)
                    else:
                        client.wait_execution(EXECUTION_ID, deadline_seconds=0.05, poll_interval=0.1)
                self.assertEqual(len(calls), 1)
                self.assertEqual(
                    raised.exception.execution_id,
                    None if operation == "fork" else EXECUTION_ID,
                )

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
    async def test_persistent_poll_overload_preserves_execution_deadline(self) -> None:
        methods: list[str] = []

        async def handler(request: httpx.Request) -> httpx.Response:
            methods.append(request.method)
            return httpx.Response(503, json={"error": "busy"})

        async with AsyncTaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
            with self.assertRaises(TaritDeadlineExceeded) as raised:
                await client.wait_execution(EXECUTION_ID, deadline_seconds=.02)
        self.assertEqual(raised.exception.execution_id, EXECUTION_ID)
        self.assertEqual(methods, ["GET"])

    async def test_transient_poll_responses_retry_without_submission_replay(self) -> None:
        statuses = iter([202, 429, 503, 504, 200])
        methods: list[str] = []

        async def handler(request: httpx.Request) -> httpx.Response:
            methods.append(request.method)
            status = next(statuses)
            return httpx.Response(status, json=execution("completed" if status == 200 else "pending")
                                  if status in {200, 202} else {"error": "busy"})

        async with AsyncTaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
            result = await client.execute(VM_ID, "once", deadline_seconds=3)
        self.assertEqual(result.status, "completed")
        self.assertEqual(methods, ["POST", "GET", "GET", "GET", "GET"])

    async def test_execute_deadline_retains_id_from_submission_through_polling(
        self,
    ) -> None:
        for stage in (
            "submission",
            "submission_decode",
            "poll",
            "poll_decode",
            "poll_timeout",
        ):
            with self.subTest(stage=stage):
                clock = Clock()
                methods = []
                parse = high_level._parsed

                def slow_parse(*args, parse=parse, stage=stage, methods=methods, clock=clock):
                    result = parse(*args)
                    if stage == f"{'submission' if len(methods) == 1 else 'poll'}_decode":
                        clock.advance(2)
                    return result

                async def handler(request: httpx.Request, methods=methods, stage=stage, clock=clock) -> httpx.Response:
                    methods.append(request.method)
                    if stage == "submission" or (request.method == "GET" and stage in ("poll", "poll_timeout")):
                        clock.advance(2)
                    if stage == "poll_timeout" and request.method == "GET":
                        raise httpx.ReadTimeout("poll response lost", request=request)
                    return httpx.Response(
                        202 if request.method == "POST" else 200,
                        json=execution("pending" if request.method == "POST" else "completed"),
                    )

                with (
                    clock.patched(),
                    patch("tarit_sdk.high_level._parsed", side_effect=slow_parse),
                ):
                    async with AsyncTaritClient(
                        "https://tarit.test",
                        "key",
                        transport=httpx.MockTransport(handler),
                    ) as client:
                        with self.assertRaises(TaritDeadlineExceeded) as raised:
                            await client.execute(VM_ID, "echo once", deadline_seconds=1)
                self.assertEqual(raised.exception.execution_id, EXECUTION_ID)
                self.assertIn(str(EXECUTION_ID), str(raised.exception))
                self.assertIsNone(raised.exception.child_id)
                self.assertEqual(
                    methods,
                    ["POST"] if stage.startswith("submission") else ["POST", "GET"],
                )

    async def test_submission_timeout_without_response_does_not_invent_execution_id(
        self,
    ) -> None:
        clock = Clock()
        calls = []

        async def handler(request: httpx.Request) -> httpx.Response:
            calls.append(request)
            clock.advance(2)
            raise httpx.ReadTimeout("submission response lost", request=request)

        with clock.patched():
            async with AsyncTaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
                with self.assertRaises(TaritDeadlineExceeded) as raised:
                    await client.execute(VM_ID, "echo once", deadline_seconds=1)
        self.assertIsNone(raised.exception.execution_id)
        self.assertIn("execution ID unavailable", str(raised.exception))
        self.assertIn("may have been accepted", str(raised.exception))
        self.assertEqual(len(calls), 1)

    async def test_execution_context_does_not_replace_transport_or_api_errors(
        self,
    ) -> None:
        failure = httpx.ReadTimeout("short network timeout")
        for operation in ("execute", "wait_execution"):
            for response in (failure, httpx.Response(403, json={"error": "forbidden"})):
                with self.subTest(operation=operation, response=response):

                    async def handler(_request: httpx.Request, response=response) -> httpx.Response:
                        if isinstance(response, Exception):
                            raise response
                        return response

                    async with AsyncTaritClient(
                        "https://tarit.test",
                        "key",
                        transport=httpx.MockTransport(handler),
                    ) as client:
                        with self.assertRaises(httpx.ReadTimeout if response is failure else TaritApiError) as raised:
                            if operation == "execute":
                                await client.execute(VM_ID, "echo once")
                            else:
                                await client.wait_execution(EXECUTION_ID)
                        if response is failure:
                            self.assertIs(raised.exception, failure)

    async def test_fork_deadline_exposes_generated_id_after_late_success(self) -> None:
        clock = Clock()
        ids = []

        async def handler(request: httpx.Request) -> httpx.Response:
            child_id = json.loads(request.content)["id"]
            ids.append(child_id)
            clock.advance(2)
            result = fork_result()
            result["vm"]["id"] = child_id
            return httpx.Response(201, json=result)

        with clock.patched():
            async with AsyncTaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
                with self.assertRaises(TaritDeadlineExceeded) as raised:
                    await client.fork(VM_ID, deadline_seconds=1)
        self.assertEqual(raised.exception.child_id, UUID(ids[0]))
        self.assertIn(ids[0], str(raised.exception))
        self.assertEqual(len(ids), 1)

    async def test_fork_transport_retry_deadline_retains_the_same_id(self) -> None:
        for supplied in (None, CHILD_ID):
            with self.subTest(child_id=supplied):
                clock = Clock()
                ids = []

                async def handler(request: httpx.Request, clock=clock, ids=ids) -> httpx.Response:
                    ids.append(json.loads(request.content)["id"])
                    clock.advance(0.1 if len(ids) == 1 else 2)
                    raise httpx.ReadTimeout("response lost", request=request)

                with clock.patched():
                    async with AsyncTaritClient(
                        "https://tarit.test",
                        "key",
                        transport=httpx.MockTransport(handler),
                    ) as client:
                        with self.assertRaises(TaritDeadlineExceeded) as raised:
                            await client.fork(VM_ID, child_id=supplied, deadline_seconds=1)
                self.assertEqual(len(ids), 2)
                self.assertEqual(ids, [str(raised.exception.child_id)] * 2)
                if supplied is not None:
                    self.assertEqual(raised.exception.child_id, supplied)

    async def test_fork_cancellation_at_deadline_exposes_generated_id(self) -> None:
        ids = []

        async def handler(request: httpx.Request) -> httpx.Response:
            ids.append(json.loads(request.content)["id"])
            await asyncio.sleep(60)
            self.fail("request was not cancelled")

        async with AsyncTaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
            with self.assertRaises(TaritDeadlineExceeded) as raised:
                await client.fork(VM_ID, deadline_seconds=0.01)
        self.assertEqual(raised.exception.child_id, UUID(ids[0]))
        self.assertIn(ids[0], str(raised.exception))

    async def test_invalid_fork_deadline_does_not_generate_or_submit_a_child(
        self,
    ) -> None:
        async def handler(_request: httpx.Request) -> httpx.Response:
            self.fail("invalid deadline must not submit a fork")

        with patch("tarit_sdk.high_level.uuid4") as new_id:
            async with AsyncTaritClient("https://tarit.test", "key", transport=httpx.MockTransport(handler)) as client:
                for value in (0, -1, float("inf"), float("nan")):
                    with self.subTest(deadline=value), self.assertRaises(ValueError):
                        await client.fork(VM_ID, deadline_seconds=value)
                new_id.assert_not_called()

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

        with (
            clock.patched(),
            patch("tarit_sdk.high_level._parsed", side_effect=slow_parse),
        ):
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
                    request: httpx.Request,
                    clock=clock,
                    operation=operation,
                    requests=requests,
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
                        with self.assertRaises(TaritDeadlineExceeded) as raised:
                            if operation == "execute":
                                await client.execute(VM_ID, "echo ok", deadline_seconds=1)
                            elif operation == "fork":
                                await client.fork(VM_ID, child_id=CHILD_ID, deadline_seconds=1)
                            else:
                                await client.wait_execution(EXECUTION_ID, deadline_seconds=1)
                self.assertEqual(len(requests), 1)
                self.assertEqual(
                    raised.exception.execution_id,
                    None if operation == "fork" else EXECUTION_ID,
                )

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
                    with self.assertRaises(TaritDeadlineExceeded) as raised:
                        if operation == "execute":
                            await client.execute(VM_ID, "echo ok", deadline_seconds=0.01)
                        elif operation == "fork":
                            await client.fork(VM_ID, deadline_seconds=0.01)
                        else:
                            await client.wait_execution(EXECUTION_ID, deadline_seconds=0.01)
                self.assertTrue(cancelled)
                self.assertEqual(
                    raised.exception.execution_id,
                    EXECUTION_ID if operation == "wait_execution" else None,
                )
                if operation == "execute":
                    self.assertIn("execution ID unavailable", str(raised.exception))

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
