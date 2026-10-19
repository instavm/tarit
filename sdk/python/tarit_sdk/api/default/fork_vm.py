from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote
from uuid import UUID

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.fork_vm_request import ForkVmRequest
from ...models.fork_vm_response import ForkVmResponse
from ...types import Response


def _get_kwargs(
    id: UUID,
    *,
    body: ForkVmRequest,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/vms/{id}/fork".format(
            id=quote(str(id), safe=""),
        ),
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Any | ForkVmResponse | None:
    if response.status_code == 200:
        response_200 = ForkVmResponse.from_dict(response.json())

        return response_200

    if response.status_code == 201:
        response_201 = ForkVmResponse.from_dict(response.json())

        return response_201

    if response.status_code == 401:
        response_401 = cast(Any, None)
        return response_401

    if response.status_code == 403:
        response_403 = cast(Any, None)
        return response_403

    if response.status_code == 404:
        response_404 = cast(Any, None)
        return response_404

    if response.status_code == 409:
        response_409 = cast(Any, None)
        return response_409

    if response.status_code == 429:
        response_429 = cast(Any, None)
        return response_429

    if response.status_code == 503:
        response_503 = cast(Any, None)
        return response_503

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[Any | ForkVmResponse]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    id: UUID,
    *,
    client: AuthenticatedClient | Client,
    body: ForkVmRequest,
) -> Response[Any | ForkVmResponse]:
    """Atomically fork a running sandbox

     Takes a bounded live snapshot of the running source, including RAM, device state, and its private
    disk upper at one fork point, then restores an isolated child with lazy UFFD memory and a new
    writable disk overlay. In fleet mode, a request received by a healthy non-owner node securely
    snapshots the source through session-fenced mTLS, localizes and verifies the artifact, requires the
    configured replica policy, and starts the child on that receiving node. Placement identifiers and
    host paths are never caller-controlled or returned. The source and child must belong to the
    authenticated tenant.

    Args:
        id (UUID):
        body (ForkVmRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | ForkVmResponse]
    """

    kwargs = _get_kwargs(
        id=id,
        body=body,
    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)


def sync(
    id: UUID,
    *,
    client: AuthenticatedClient | Client,
    body: ForkVmRequest,
) -> Any | ForkVmResponse | None:
    """Atomically fork a running sandbox

     Takes a bounded live snapshot of the running source, including RAM, device state, and its private
    disk upper at one fork point, then restores an isolated child with lazy UFFD memory and a new
    writable disk overlay. In fleet mode, a request received by a healthy non-owner node securely
    snapshots the source through session-fenced mTLS, localizes and verifies the artifact, requires the
    configured replica policy, and starts the child on that receiving node. Placement identifiers and
    host paths are never caller-controlled or returned. The source and child must belong to the
    authenticated tenant.

    Args:
        id (UUID):
        body (ForkVmRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | ForkVmResponse
    """

    return sync_detailed(
        id=id,
        client=client,
        body=body,
    ).parsed


async def asyncio_detailed(
    id: UUID,
    *,
    client: AuthenticatedClient | Client,
    body: ForkVmRequest,
) -> Response[Any | ForkVmResponse]:
    """Atomically fork a running sandbox

     Takes a bounded live snapshot of the running source, including RAM, device state, and its private
    disk upper at one fork point, then restores an isolated child with lazy UFFD memory and a new
    writable disk overlay. In fleet mode, a request received by a healthy non-owner node securely
    snapshots the source through session-fenced mTLS, localizes and verifies the artifact, requires the
    configured replica policy, and starts the child on that receiving node. Placement identifiers and
    host paths are never caller-controlled or returned. The source and child must belong to the
    authenticated tenant.

    Args:
        id (UUID):
        body (ForkVmRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | ForkVmResponse]
    """

    kwargs = _get_kwargs(
        id=id,
        body=body,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    id: UUID,
    *,
    client: AuthenticatedClient | Client,
    body: ForkVmRequest,
) -> Any | ForkVmResponse | None:
    """Atomically fork a running sandbox

     Takes a bounded live snapshot of the running source, including RAM, device state, and its private
    disk upper at one fork point, then restores an isolated child with lazy UFFD memory and a new
    writable disk overlay. In fleet mode, a request received by a healthy non-owner node securely
    snapshots the source through session-fenced mTLS, localizes and verifies the artifact, requires the
    configured replica policy, and starts the child on that receiving node. Placement identifiers and
    host paths are never caller-controlled or returned. The source and child must belong to the
    authenticated tenant.

    Args:
        id (UUID):
        body (ForkVmRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | ForkVmResponse
    """

    return (
        await asyncio_detailed(
            id=id,
            client=client,
            body=body,
        )
    ).parsed
