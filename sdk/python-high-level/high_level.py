from __future__ import annotations

import asyncio
import json
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Self, TypeVar
from uuid import UUID, uuid4

import httpx

from .api.default import execute_async as execute_async_api
from .api.default import fork_vm as fork_vm_api
from .api.default import get_execution as get_execution_api
from .client import AuthenticatedClient
from .models.execute_request import ExecuteRequest
from .models.execution_record import ExecutionRecord
from .models.fork_vm_request import ForkVmRequest
from .models.fork_vm_response import ForkVmResponse
from .types import Response

T = TypeVar("T")
RETRYABLE_STATUS = {429, 502, 503, 504}
TERMINAL_EXECUTION_STATUS = {"completed", "failed"}


@dataclass(frozen=True)
class TaritApiError(Exception):
    operation: str
    status_code: int
    message: str

    def __str__(self) -> str:
        return f"{self.operation} failed with HTTP {self.status_code}: {self.message}"


class TaritDeadlineExceeded(TimeoutError):
    pass


def _error_message(content: bytes) -> str:
    try:
        body: object = json.loads(content)
    except (UnicodeDecodeError, json.JSONDecodeError):
        return content.decode("utf-8", errors="replace")[:1024] or "empty response"
    if isinstance(body, dict):
        value = body.get("error")
        if isinstance(value, str):
            return value[:1024]
    return json.dumps(body, separators=(",", ":"))[:1024]


def _parsed(response: Response[object], expected: type[T], operation: str) -> T:
    if isinstance(response.parsed, expected):
        return response.parsed
    raise TaritApiError(
        operation=operation,
        status_code=int(response.status_code),
        message=_error_message(response.content),
    )


def _retry_delay(attempt: int) -> float:
    delays = (0.1, 0.2, 0.4, 0.8, 1.0)
    return delays[min(max(attempt, 0), len(delays) - 1)]


class TaritClient:
    """Deadline-bounded helpers over the generated synchronous client."""

    def __init__(
        self,
        base_url: str,
        api_key: str,
        *,
        request_timeout: float = 30.0,
        verify_ssl: bool = True,
        transport: httpx.BaseTransport | None = None,
    ) -> None:
        if not api_key:
            raise ValueError("api_key must not be empty")
        self.raw = AuthenticatedClient(
            base_url=base_url.rstrip("/"),
            token=api_key,
            prefix="",
            auth_header_name="X-API-Key",
            timeout=httpx.Timeout(request_timeout),
            verify_ssl=verify_ssl,
            raise_on_unexpected_status=False,
            httpx_args={"transport": transport} if transport is not None else {},
        )

    def close(self) -> None:
        client = self.raw.get_httpx_client()
        if not client.is_closed:
            client.close()

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_args: object) -> None:
        self.close()

    def fork(
        self,
        vm_id: UUID,
        *,
        child_id: UUID | None = None,
        deadline_seconds: float = 30.0,
    ) -> ForkVmResponse:
        """Fork with one stable child id across transport and overload retries."""
        child_id = child_id or uuid4()
        deadline = time.monotonic() + deadline_seconds
        attempt = 0
        while True:
            try:
                response = fork_vm_api.sync_detailed(
                    vm_id,
                    client=self.raw,
                    body=ForkVmRequest(id=child_id),
                )
            except (httpx.TimeoutException, httpx.NetworkError):
                response = None
            if response is not None and int(response.status_code) not in RETRYABLE_STATUS:
                return _parsed(response, ForkVmResponse, "fork VM")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TaritDeadlineExceeded(f"fork VM {vm_id} exceeded its deadline")
            time.sleep(min(_retry_delay(attempt), remaining))
            attempt += 1

    def execute(
        self,
        vm_id: UUID,
        command: str,
        *,
        timeout_ms: int = 30_000,
        deadline_seconds: float | None = None,
        poll_interval: float = 0.1,
    ) -> ExecutionRecord:
        """Submit an asynchronous execution and wait for its terminal record."""
        response = execute_async_api.sync_detailed(
            client=self.raw,
            body=ExecuteRequest(vm_id=vm_id, command=command, timeout_ms=timeout_ms),
        )
        record = _parsed(response, ExecutionRecord, "execute command")
        return self.wait_execution(
            record.id,
            deadline_seconds=deadline_seconds or max(timeout_ms / 1000 + 5, 5),
            poll_interval=poll_interval,
        )

    def wait_execution(
        self,
        execution_id: UUID,
        *,
        deadline_seconds: float = 35.0,
        poll_interval: float = 0.1,
    ) -> ExecutionRecord:
        deadline = time.monotonic() + deadline_seconds
        while True:
            response = get_execution_api.sync_detailed(execution_id, client=self.raw)
            record = _parsed(response, ExecutionRecord, "get execution")
            if record.status in TERMINAL_EXECUTION_STATUS:
                return record
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TaritDeadlineExceeded(f"execution {execution_id} exceeded its deadline")
            time.sleep(min(poll_interval, remaining))


class AsyncTaritClient:
    """Deadline-bounded helpers over the generated asynchronous client."""

    def __init__(
        self,
        base_url: str,
        api_key: str,
        *,
        request_timeout: float = 30.0,
        verify_ssl: bool = True,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        if not api_key:
            raise ValueError("api_key must not be empty")
        self.raw = AuthenticatedClient(
            base_url=base_url.rstrip("/"),
            token=api_key,
            prefix="",
            auth_header_name="X-API-Key",
            timeout=httpx.Timeout(request_timeout),
            verify_ssl=verify_ssl,
            raise_on_unexpected_status=False,
            httpx_args={"transport": transport} if transport is not None else {},
        )

    async def close(self) -> None:
        client = self.raw.get_async_httpx_client()
        if not client.is_closed:
            await client.aclose()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *_args: object) -> None:
        await self.close()

    async def _retry(
        self,
        operation: str,
        deadline_seconds: float,
        call: Callable[[], Awaitable[Response[T]]],
    ) -> Response[T]:
        deadline = time.monotonic() + deadline_seconds
        attempt = 0
        while True:
            try:
                response = await call()
            except (httpx.TimeoutException, httpx.NetworkError):
                response = None
            if response is not None and int(response.status_code) not in RETRYABLE_STATUS:
                return response
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TaritDeadlineExceeded(f"{operation} exceeded its deadline")
            await asyncio.sleep(min(_retry_delay(attempt), remaining))
            attempt += 1

    async def fork(
        self,
        vm_id: UUID,
        *,
        child_id: UUID | None = None,
        deadline_seconds: float = 30.0,
    ) -> ForkVmResponse:
        child_id = child_id or uuid4()
        response = await self._retry(
            f"fork VM {vm_id}",
            deadline_seconds,
            lambda: fork_vm_api.asyncio_detailed(
                vm_id,
                client=self.raw,
                body=ForkVmRequest(id=child_id),
            ),
        )
        return _parsed(response, ForkVmResponse, "fork VM")

    async def execute(
        self,
        vm_id: UUID,
        command: str,
        *,
        timeout_ms: int = 30_000,
        deadline_seconds: float | None = None,
        poll_interval: float = 0.1,
    ) -> ExecutionRecord:
        response = await execute_async_api.asyncio_detailed(
            client=self.raw,
            body=ExecuteRequest(vm_id=vm_id, command=command, timeout_ms=timeout_ms),
        )
        record = _parsed(response, ExecutionRecord, "execute command")
        return await self.wait_execution(
            record.id,
            deadline_seconds=deadline_seconds or max(timeout_ms / 1000 + 5, 5),
            poll_interval=poll_interval,
        )

    async def wait_execution(
        self,
        execution_id: UUID,
        *,
        deadline_seconds: float = 35.0,
        poll_interval: float = 0.1,
    ) -> ExecutionRecord:
        deadline = time.monotonic() + deadline_seconds
        while True:
            response = await get_execution_api.asyncio_detailed(execution_id, client=self.raw)
            record = _parsed(response, ExecutionRecord, "get execution")
            if record.status in TERMINAL_EXECUTION_STATUS:
                return record
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TaritDeadlineExceeded(f"execution {execution_id} exceeded its deadline")
            await asyncio.sleep(min(poll_interval, remaining))
