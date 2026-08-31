from http import HTTPStatus
from typing import Any, cast

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.restore_request import RestoreRequest
from ...models.vm_record import VmRecord
from ...types import Response


def _get_kwargs(
    *,
    body: RestoreRequest,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/restore",
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Any | VmRecord | None:
    if response.status_code == 201:
        response_201 = VmRecord.from_dict(response.json())

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
    body: RestoreRequest,
) -> Response[Any | VmRecord]:
    """Restore sandbox from a snapshot

     Restores a VM from an opaque snapshot handle. The control plane resolves its private host and
    storage locator; tenant VM quotas apply.

    Args:
        body (RestoreRequest):

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
    body: RestoreRequest,
) -> Any | VmRecord | None:
    """Restore sandbox from a snapshot

     Restores a VM from an opaque snapshot handle. The control plane resolves its private host and
    storage locator; tenant VM quotas apply.

    Args:
        body (RestoreRequest):

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
    body: RestoreRequest,
) -> Response[Any | VmRecord]:
    """Restore sandbox from a snapshot

     Restores a VM from an opaque snapshot handle. The control plane resolves its private host and
    storage locator; tenant VM quotas apply.

    Args:
        body (RestoreRequest):

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
    body: RestoreRequest,
) -> Any | VmRecord | None:
    """Restore sandbox from a snapshot

     Restores a VM from an opaque snapshot handle. The control plane resolves its private host and
    storage locator; tenant VM quotas apply.

    Args:
        body (RestoreRequest):

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
