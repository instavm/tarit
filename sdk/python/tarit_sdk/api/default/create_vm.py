from http import HTTPStatus
from typing import Any, cast

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.create_vm_request import CreateVmRequest
from ...models.vm_record import VmRecord
from ...types import Response


def _get_kwargs(
    *,
    body: CreateVmRequest,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/vms",
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Any | VmRecord | None:
    if response.status_code == 201:
        response_201 = VmRecord.from_dict(response.json())

        return response_201

    if response.status_code == 400:
        response_400 = cast(Any, None)
        return response_400

    if response.status_code == 401:
        response_401 = cast(Any, None)
        return response_401

    if response.status_code == 403:
        response_403 = cast(Any, None)
        return response_403

    if response.status_code == 409:
        response_409 = cast(Any, None)
        return response_409

    if response.status_code == 422:
        response_422 = cast(Any, None)
        return response_422

    if response.status_code == 429:
        response_429 = cast(Any, None)
        return response_429

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Response[Any | VmRecord]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: AuthenticatedClient | Client,
    body: CreateVmRequest,
) -> Response[Any | VmRecord]:
    """Create sandbox

     Creates a VM owned by the caller's tenant. Persistent volumes are acquired before VMM startup with
    an exclusive durable writer fence; local volumes pin placement to this exact host and bypass warm-
    pool reuse. Non-admin callers may set `image` (`name[:tag]`) or omit paths for node defaults outside
    production, but cannot set raw `kernel_path` or `rootfs_path`.

    Args:
        body (CreateVmRequest): All fields optional; defaults come from node configuration.
            Persistent block attachments are explicit, ordered after the root disk, exact-host
            constrained for local_block, and remain external across snapshot/hibernate operations.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | VmRecord]
    """

    kwargs = _get_kwargs(
        body=body,
    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)


def sync(
    *,
    client: AuthenticatedClient | Client,
    body: CreateVmRequest,
) -> Any | VmRecord | None:
    """Create sandbox

     Creates a VM owned by the caller's tenant. Persistent volumes are acquired before VMM startup with
    an exclusive durable writer fence; local volumes pin placement to this exact host and bypass warm-
    pool reuse. Non-admin callers may set `image` (`name[:tag]`) or omit paths for node defaults outside
    production, but cannot set raw `kernel_path` or `rootfs_path`.

    Args:
        body (CreateVmRequest): All fields optional; defaults come from node configuration.
            Persistent block attachments are explicit, ordered after the root disk, exact-host
            constrained for local_block, and remain external across snapshot/hibernate operations.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | VmRecord
    """

    return sync_detailed(
        client=client,
        body=body,
    ).parsed


async def asyncio_detailed(
    *,
    client: AuthenticatedClient | Client,
    body: CreateVmRequest,
) -> Response[Any | VmRecord]:
    """Create sandbox

     Creates a VM owned by the caller's tenant. Persistent volumes are acquired before VMM startup with
    an exclusive durable writer fence; local volumes pin placement to this exact host and bypass warm-
    pool reuse. Non-admin callers may set `image` (`name[:tag]`) or omit paths for node defaults outside
    production, but cannot set raw `kernel_path` or `rootfs_path`.

    Args:
        body (CreateVmRequest): All fields optional; defaults come from node configuration.
            Persistent block attachments are explicit, ordered after the root disk, exact-host
            constrained for local_block, and remain external across snapshot/hibernate operations.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | VmRecord]
    """

    kwargs = _get_kwargs(
        body=body,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    *,
    client: AuthenticatedClient | Client,
    body: CreateVmRequest,
) -> Any | VmRecord | None:
    """Create sandbox

     Creates a VM owned by the caller's tenant. Persistent volumes are acquired before VMM startup with
    an exclusive durable writer fence; local volumes pin placement to this exact host and bypass warm-
    pool reuse. Non-admin callers may set `image` (`name[:tag]`) or omit paths for node defaults outside
    production, but cannot set raw `kernel_path` or `rootfs_path`.

    Args:
        body (CreateVmRequest): All fields optional; defaults come from node configuration.
            Persistent block attachments are explicit, ordered after the root disk, exact-host
            constrained for local_block, and remain external across snapshot/hibernate operations.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | VmRecord
    """

    return (
        await asyncio_detailed(
            client=client,
            body=body,
        )
    ).parsed
