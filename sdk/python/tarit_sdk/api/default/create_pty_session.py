from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote
from uuid import UUID

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.create_pty_session_body import CreatePtySessionBody
from ...models.create_pty_session_response_201 import CreatePtySessionResponse201
from ...types import Response


def _get_kwargs(
    id: UUID,
    *,
    body: CreatePtySessionBody,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/vms/{id}/pty/sessions".format(
            id=quote(str(id), safe=""),
        ),
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Any | CreatePtySessionResponse201 | None:
    if response.status_code == 201:
        response_201 = CreatePtySessionResponse201.from_dict(response.json())

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

    if response.status_code == 404:
        response_404 = cast(Any, None)
        return response_404

    if response.status_code == 409:
        response_409 = cast(Any, None)
        return response_409

    if response.status_code == 429:
        response_429 = cast(Any, None)
        return response_429

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[Any | CreatePtySessionResponse201]:
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
    body: CreatePtySessionBody,
) -> Response[Any | CreatePtySessionResponse201]:
    """Create PTY session

     Registers an interactive PTY session for the VM. Tokens are one-time and expire after five minutes.
    At most 32 pending sessions may exist per VM. PTY routes operate on the VM's owning node only; on a
    non-owner node they return 409.

    Args:
        id (UUID):
        body (CreatePtySessionBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | CreatePtySessionResponse201]
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
    body: CreatePtySessionBody,
) -> Any | CreatePtySessionResponse201 | None:
    """Create PTY session

     Registers an interactive PTY session for the VM. Tokens are one-time and expire after five minutes.
    At most 32 pending sessions may exist per VM. PTY routes operate on the VM's owning node only; on a
    non-owner node they return 409.

    Args:
        id (UUID):
        body (CreatePtySessionBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | CreatePtySessionResponse201
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
    body: CreatePtySessionBody,
) -> Response[Any | CreatePtySessionResponse201]:
    """Create PTY session

     Registers an interactive PTY session for the VM. Tokens are one-time and expire after five minutes.
    At most 32 pending sessions may exist per VM. PTY routes operate on the VM's owning node only; on a
    non-owner node they return 409.

    Args:
        id (UUID):
        body (CreatePtySessionBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | CreatePtySessionResponse201]
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
    body: CreatePtySessionBody,
) -> Any | CreatePtySessionResponse201 | None:
    """Create PTY session

     Registers an interactive PTY session for the VM. Tokens are one-time and expire after five minutes.
    At most 32 pending sessions may exist per VM. PTY routes operate on the VM's owning node only; on a
    non-owner node they return 409.

    Args:
        id (UUID):
        body (CreatePtySessionBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | CreatePtySessionResponse201
    """

    return (
        await asyncio_detailed(
            id=id,
            client=client,
            body=body,
        )
    ).parsed
