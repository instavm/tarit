from __future__ import annotations

import asyncio
import json
import math
import ssl
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Self, TypeVar
from urllib.parse import quote, urlencode, urlsplit, urlunsplit
from uuid import UUID, uuid4

import httpx
from websockets.asyncio.client import ClientConnection as AsyncWebSocketConnection
from websockets.asyncio.client import connect as async_websocket_connect
from websockets.exceptions import ConnectionClosed
from websockets.sync.client import ClientConnection as SyncWebSocketConnection
from websockets.sync.client import connect as sync_websocket_connect

from .api.default import delete_pty_session as delete_pty_session_api
from .api.default import execute_async as execute_async_api
from .api.default import fork_vm as fork_vm_api
from .api.default import get_execution as get_execution_api
from .client import AuthenticatedClient
from .models.create_pty_session_body import CreatePtySessionBody
from .models.create_pty_session_response_201 import CreatePtySessionResponse201
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


class TaritPtyClosed(ConnectionError):
    pass


class TaritPtyProtocolError(ValueError):
    pass


class TaritPtyConnectionError(ConnectionError):
    pass


@dataclass(frozen=True)
class PtyData:
    data: bytes


@dataclass(frozen=True)
class PtyExit:
    exit_code: int


PtyMessage = PtyData | PtyExit


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


def _validate_pty_options(cols: int, rows: int, deadline_seconds: float, max_message_bytes: int) -> None:
    if not 1 <= cols <= 65_535 or not 1 <= rows <= 65_535:
        raise ValueError("PTY cols and rows must be between 1 and 65535")
    if not math.isfinite(deadline_seconds) or deadline_seconds <= 0:
        raise ValueError("PTY deadline_seconds must be greater than zero")
    if max_message_bytes <= 0:
        raise ValueError("PTY max_message_bytes must be greater than zero")


def _pty_request(cols: int, rows: int, shell: str | None) -> CreatePtySessionBody:
    if shell is None:
        return CreatePtySessionBody(cols=cols, rows=rows)
    return CreatePtySessionBody(cols=cols, rows=rows, shell=shell)


def _pty_session_from_response(response: httpx.Response) -> CreatePtySessionResponse201:
    if response.status_code != 201:
        raise TaritApiError(
            operation="create PTY session",
            status_code=response.status_code,
            message=_error_message(response.content),
        )
    try:
        return CreatePtySessionResponse201.from_dict(response.json())
    except (json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
        raise TaritPtyProtocolError("create PTY session returned an invalid response") from error


def _pty_websocket_url(base_url: str, vm_id: UUID, pty_id: UUID, token: str) -> str:
    parts = urlsplit(base_url)
    if parts.scheme == "http":
        scheme = "ws"
    elif parts.scheme == "https":
        scheme = "wss"
    else:
        raise ValueError("base_url must use http or https")
    base_path = parts.path.rstrip("/")
    path = f"{base_path}/v1/vms/{quote(str(vm_id), safe='')}/pty/{quote(str(pty_id), safe='')}/connect"
    return urlunsplit((scheme, parts.netloc, path, urlencode({"token": token}), ""))


def _websocket_ssl_context(base_url: str, verify_ssl: bool) -> ssl.SSLContext | None:
    if urlsplit(base_url).scheme != "https" or verify_ssl:
        return None
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    return context


def _parse_pty_message(message: str | bytes) -> PtyMessage:
    if isinstance(message, bytes):
        return PtyData(message)
    try:
        control = json.loads(message)
    except json.JSONDecodeError as error:
        raise TaritPtyProtocolError("PTY server sent malformed control JSON") from error
    if not isinstance(control, dict) or control.get("type") != "exit":
        raise TaritPtyProtocolError("PTY server sent an unknown control message")
    exit_code = control.get("exit_code")
    if not isinstance(exit_code, int) or isinstance(exit_code, bool) or not -(2**31) <= exit_code < 2**31:
        raise TaritPtyProtocolError("PTY server sent an invalid exit code")
    return PtyExit(exit_code)


class PtyConnection:
    """One authenticated PTY WebSocket plus its server-side session lease."""

    def __init__(
        self,
        client: TaritClient,
        vm_id: UUID,
        pty_id: UUID,
        websocket: SyncWebSocketConnection,
    ) -> None:
        self.vm_id = vm_id
        self.pty_id = pty_id
        self._client = client
        self._websocket = websocket
        self._closed = False

    def write(self, data: bytes | str) -> None:
        payload = data.encode() if isinstance(data, str) else data
        self._websocket.send(payload)

    def resize(self, cols: int, rows: int) -> None:
        _validate_pty_options(cols, rows, 1, 1)
        self._websocket.send(json.dumps({"type": "resize", "cols": cols, "rows": rows}, separators=(",", ":")))

    def read(self, *, timeout: float | None = 30.0) -> PtyMessage:
        try:
            return _parse_pty_message(self._websocket.recv(timeout=timeout))
        except TimeoutError as error:
            raise TaritDeadlineExceeded(f"PTY session {self.pty_id} read exceeded its deadline") from error
        except ConnectionClosed as error:
            raise TaritPtyClosed(f"PTY session {self.pty_id} closed before an exit frame") from error

    def close(self, *, delete_session: bool = True) -> None:
        if self._closed:
            return
        self._closed = True
        close_error: Exception | None = None
        try:
            self._websocket.close()
        except Exception as error:
            close_error = error
        try:
            if delete_session:
                self._client._delete_pty_session(self.vm_id, self.pty_id)
        finally:
            if close_error is not None:
                raise TaritPtyConnectionError("PTY WebSocket close failed") from close_error

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_args: object) -> None:
        self.close()


class AsyncPtyConnection:
    """Asyncio variant of :class:`PtyConnection`."""

    def __init__(
        self,
        client: AsyncTaritClient,
        vm_id: UUID,
        pty_id: UUID,
        websocket: AsyncWebSocketConnection,
    ) -> None:
        self.vm_id = vm_id
        self.pty_id = pty_id
        self._client = client
        self._websocket = websocket
        self._closed = False

    async def write(self, data: bytes | str) -> None:
        payload = data.encode() if isinstance(data, str) else data
        await self._websocket.send(payload)

    async def resize(self, cols: int, rows: int) -> None:
        _validate_pty_options(cols, rows, 1, 1)
        await self._websocket.send(json.dumps({"type": "resize", "cols": cols, "rows": rows}, separators=(",", ":")))

    async def read(self, *, timeout: float | None = 30.0) -> PtyMessage:
        try:
            if timeout is None:
                message = await self._websocket.recv()
            else:
                message = await asyncio.wait_for(self._websocket.recv(), timeout)
            return _parse_pty_message(message)
        except TimeoutError as error:
            raise TaritDeadlineExceeded(f"PTY session {self.pty_id} read exceeded its deadline") from error
        except ConnectionClosed as error:
            raise TaritPtyClosed(f"PTY session {self.pty_id} closed before an exit frame") from error

    async def close(self, *, delete_session: bool = True) -> None:
        if self._closed:
            return
        self._closed = True
        close_error: Exception | None = None
        try:
            await self._websocket.close()
        except Exception as error:
            close_error = error
        try:
            if delete_session:
                await self._client._delete_pty_session(self.vm_id, self.pty_id)
        finally:
            if close_error is not None:
                raise TaritPtyConnectionError("PTY WebSocket close failed") from close_error

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *_args: object) -> None:
        await self.close()


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
        self._base_url = base_url.rstrip("/")
        self._verify_ssl = verify_ssl
        self.raw = AuthenticatedClient(
            base_url=self._base_url,
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

    def _delete_pty_session(self, vm_id: UUID, pty_id: UUID) -> None:
        response = delete_pty_session_api.sync_detailed(vm_id, pty_id, client=self.raw)
        if int(response.status_code) not in {204, 404}:
            raise TaritApiError(
                operation="delete PTY session",
                status_code=int(response.status_code),
                message=_error_message(response.content),
            )

    def open_pty(
        self,
        vm_id: UUID,
        *,
        cols: int = 80,
        rows: int = 24,
        shell: str | None = None,
        deadline_seconds: float = 30.0,
        max_message_bytes: int = 1024 * 1024,
    ) -> PtyConnection:
        """Activate a VM if needed, create a PTY lease, and attach its WebSocket."""
        _validate_pty_options(cols, rows, deadline_seconds, max_message_bytes)
        deadline = time.monotonic() + deadline_seconds
        try:
            response = self.raw.get_httpx_client().post(
                f"/v1/vms/{vm_id}/pty/sessions",
                json=_pty_request(cols, rows, shell).to_dict(),
                timeout=deadline_seconds,
            )
        except httpx.TimeoutException as error:
            raise TaritDeadlineExceeded(f"open PTY for VM {vm_id} exceeded its deadline") from error
        session = _pty_session_from_response(response)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            self._delete_pty_session(vm_id, session.pty_id)
            raise TaritDeadlineExceeded(f"open PTY for VM {vm_id} exceeded its deadline")
        url = _pty_websocket_url(self._base_url, vm_id, session.pty_id, session.connect_token)
        try:
            websocket = sync_websocket_connect(
                url,
                open_timeout=remaining,
                close_timeout=min(remaining, 5.0),
                max_size=max_message_bytes,
                ssl=_websocket_ssl_context(self._base_url, self._verify_ssl),
            )
        except Exception as error:
            try:
                self._delete_pty_session(vm_id, session.pty_id)
            except Exception:
                pass
            raise TaritPtyConnectionError("PTY WebSocket connection failed") from error
        return PtyConnection(self, vm_id, session.pty_id, websocket)

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
        self._base_url = base_url.rstrip("/")
        self._verify_ssl = verify_ssl
        self.raw = AuthenticatedClient(
            base_url=self._base_url,
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

    async def _delete_pty_session(self, vm_id: UUID, pty_id: UUID) -> None:
        response = await delete_pty_session_api.asyncio_detailed(vm_id, pty_id, client=self.raw)
        if int(response.status_code) not in {204, 404}:
            raise TaritApiError(
                operation="delete PTY session",
                status_code=int(response.status_code),
                message=_error_message(response.content),
            )

    async def open_pty(
        self,
        vm_id: UUID,
        *,
        cols: int = 80,
        rows: int = 24,
        shell: str | None = None,
        deadline_seconds: float = 30.0,
        max_message_bytes: int = 1024 * 1024,
    ) -> AsyncPtyConnection:
        """Activate a VM if needed, create a PTY lease, and attach its WebSocket."""
        _validate_pty_options(cols, rows, deadline_seconds, max_message_bytes)
        deadline = time.monotonic() + deadline_seconds
        try:
            response = await self.raw.get_async_httpx_client().post(
                f"/v1/vms/{vm_id}/pty/sessions",
                json=_pty_request(cols, rows, shell).to_dict(),
                timeout=deadline_seconds,
            )
        except httpx.TimeoutException as error:
            raise TaritDeadlineExceeded(f"open PTY for VM {vm_id} exceeded its deadline") from error
        session = _pty_session_from_response(response)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            await self._delete_pty_session(vm_id, session.pty_id)
            raise TaritDeadlineExceeded(f"open PTY for VM {vm_id} exceeded its deadline")
        url = _pty_websocket_url(self._base_url, vm_id, session.pty_id, session.connect_token)
        try:
            websocket = await async_websocket_connect(
                url,
                open_timeout=remaining,
                close_timeout=min(remaining, 5.0),
                max_size=max_message_bytes,
                ssl=_websocket_ssl_context(self._base_url, self._verify_ssl),
            )
        except Exception as error:
            try:
                await self._delete_pty_session(vm_id, session.pty_id)
            except Exception:
                pass
            raise TaritPtyConnectionError("PTY WebSocket connection failed") from error
        return AsyncPtyConnection(self, vm_id, session.pty_id, websocket)

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
