from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote
from uuid import UUID

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.snapshot_vm_body import SnapshotVmBody
from ...models.snapshot_vm_response_200 import SnapshotVmResponse200
from ...types import Response


def _get_kwargs(
    id: UUID,
    *,
    body: SnapshotVmBody,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/vms/{id}/snapshot".format(
            id=quote(str(id), safe=""),
        ),
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Any | SnapshotVmResponse200 | None:
    if response.status_code == 200:
        response_200 = SnapshotVmResponse200.from_dict(response.json())

        return response_200

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

    if response.status_code == 422:
        response_422 = cast(Any, None)
        return response_422

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[Any | SnapshotVmResponse200]:
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
    body: SnapshotVmBody,
) -> Response[Any | SnapshotVmResponse200]:
    """Snapshot sandbox

     User keys can snapshot only their tenant's VMs; admin keys can snapshot any VM. A running VM uses
    the bounded live pre-copy path; RAM, device state, and the private disk upper are captured at one
    atomic final-stop boundary. The response is an opaque handle; paths and physical host identity
    remain private.

    Args:
        id (UUID):
        body (SnapshotVmBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | SnapshotVmResponse200]
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
    body: SnapshotVmBody,
) -> Any | SnapshotVmResponse200 | None:
    """Snapshot sandbox

     User keys can snapshot only their tenant's VMs; admin keys can snapshot any VM. A running VM uses
    the bounded live pre-copy path; RAM, device state, and the private disk upper are captured at one
    atomic final-stop boundary. The response is an opaque handle; paths and physical host identity
    remain private.

    Args:
        id (UUID):
        body (SnapshotVmBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | SnapshotVmResponse200
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
    body: SnapshotVmBody,
) -> Response[Any | SnapshotVmResponse200]:
    """Snapshot sandbox

     User keys can snapshot only their tenant's VMs; admin keys can snapshot any VM. A running VM uses
    the bounded live pre-copy path; RAM, device state, and the private disk upper are captured at one
    atomic final-stop boundary. The response is an opaque handle; paths and physical host identity
    remain private.

    Args:
        id (UUID):
        body (SnapshotVmBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | SnapshotVmResponse200]
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
    body: SnapshotVmBody,
) -> Any | SnapshotVmResponse200 | None:
    """Snapshot sandbox

     User keys can snapshot only their tenant's VMs; admin keys can snapshot any VM. A running VM uses
    the bounded live pre-copy path; RAM, device state, and the private disk upper are captured at one
    atomic final-stop boundary. The response is an opaque handle; paths and physical host identity
    remain private.

    Args:
        id (UUID):
        body (SnapshotVmBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | SnapshotVmResponse200
    """

    return (
        await asyncio_detailed(
            id=id,
            client=client,
            body=body,
        )
    ).parsed
