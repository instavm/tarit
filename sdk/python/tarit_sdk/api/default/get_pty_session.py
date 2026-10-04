from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote
from uuid import UUID

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.pty_session_response import PtySessionResponse
from ...types import Response


def _get_kwargs(
    id: UUID,
    pty_id: UUID,
) -> dict[str, Any]:

    _kwargs: dict[str, Any] = {
        "method": "get",
        "url": "/v1/vms/{id}/pty/sessions/{pty_id}".format(
            id=quote(str(id), safe=""),
            pty_id=quote(str(pty_id), safe=""),
        ),
    }

    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Any | PtySessionResponse | None:
    if response.status_code == 200:
        response_200 = PtySessionResponse.from_dict(response.json())

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

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[Any | PtySessionResponse]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    id: UUID,
    pty_id: UUID,
    *,
    client: AuthenticatedClient | Client,
) -> Response[Any | PtySessionResponse]:
    """Get PTY session

    Args:
        id (UUID):
        pty_id (UUID):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | PtySessionResponse]
    """

    kwargs = _get_kwargs(
        id=id,
        pty_id=pty_id,
    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)


def sync(
    id: UUID,
    pty_id: UUID,
    *,
    client: AuthenticatedClient | Client,
) -> Any | PtySessionResponse | None:
    """Get PTY session

    Args:
        id (UUID):
        pty_id (UUID):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | PtySessionResponse
    """

    return sync_detailed(
        id=id,
        pty_id=pty_id,
        client=client,
    ).parsed


async def asyncio_detailed(
    id: UUID,
    pty_id: UUID,
    *,
    client: AuthenticatedClient | Client,
) -> Response[Any | PtySessionResponse]:
    """Get PTY session

    Args:
        id (UUID):
        pty_id (UUID):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | PtySessionResponse]
    """

    kwargs = _get_kwargs(
        id=id,
        pty_id=pty_id,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    id: UUID,
    pty_id: UUID,
    *,
    client: AuthenticatedClient | Client,
) -> Any | PtySessionResponse | None:
    """Get PTY session

    Args:
        id (UUID):
        pty_id (UUID):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | PtySessionResponse
    """

    return (
        await asyncio_detailed(
            id=id,
            pty_id=pty_id,
            client=client,
        )
    ).parsed
