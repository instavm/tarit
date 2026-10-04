from http import HTTPStatus
from typing import Any, cast

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.create_volume_request import CreateVolumeRequest
from ...models.volume import Volume
from ...types import Response


def _get_kwargs(
    *,
    body: CreateVolumeRequest,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/volumes",
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Any | Volume | None:
    if response.status_code == 200:
        response_200 = Volume.from_dict(response.json())

        return response_200

    if response.status_code == 201:
        response_201 = Volume.from_dict(response.json())

        return response_201

    if response.status_code == 400:
        response_400 = cast(Any, None)
        return response_400

    if response.status_code == 401:
        response_401 = cast(Any, None)
        return response_401

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


def _build_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Response[Any | Volume]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: AuthenticatedClient | Client,
    body: CreateVolumeRequest,
) -> Response[Any | Volume]:
    """Create a persistent volume

     Creates an opaque tenant-owned volume. Exact replay with the same id and immutable properties is
    idempotent. The initial local-block provider is exact-host constrained; provider paths are private.

    Args:
        body (CreateVolumeRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | Volume]
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
    body: CreateVolumeRequest,
) -> Any | Volume | None:
    """Create a persistent volume

     Creates an opaque tenant-owned volume. Exact replay with the same id and immutable properties is
    idempotent. The initial local-block provider is exact-host constrained; provider paths are private.

    Args:
        body (CreateVolumeRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | Volume
    """

    return sync_detailed(
        client=client,
        body=body,
    ).parsed


async def asyncio_detailed(
    *,
    client: AuthenticatedClient | Client,
    body: CreateVolumeRequest,
) -> Response[Any | Volume]:
    """Create a persistent volume

     Creates an opaque tenant-owned volume. Exact replay with the same id and immutable properties is
    idempotent. The initial local-block provider is exact-host constrained; provider paths are private.

    Args:
        body (CreateVolumeRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | Volume]
    """

    kwargs = _get_kwargs(
        body=body,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    *,
    client: AuthenticatedClient | Client,
    body: CreateVolumeRequest,
) -> Any | Volume | None:
    """Create a persistent volume

     Creates an opaque tenant-owned volume. Exact replay with the same id and immutable properties is
    idempotent. The initial local-block provider is exact-host constrained; provider paths are private.

    Args:
        body (CreateVolumeRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | Volume
    """

    return (
        await asyncio_detailed(
            client=client,
            body=body,
        )
    ).parsed
