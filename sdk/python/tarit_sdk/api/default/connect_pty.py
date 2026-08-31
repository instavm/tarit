from http import HTTPStatus
from typing import Any
from urllib.parse import quote
from uuid import UUID

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...types import UNSET, Response


def _get_kwargs(
    id: UUID,
    pty_id: UUID,
    *,
    token: str,
) -> dict[str, Any]:

    params: dict[str, Any] = {}

    params["token"] = token

    params = {k: v for k, v in params.items() if v is not UNSET and v is not None}

    _kwargs: dict[str, Any] = {
        "method": "get",
        "url": "/v1/vms/{id}/pty/{pty_id}/connect".format(
            id=quote(str(id), safe=""),
            pty_id=quote(str(pty_id), safe=""),
        ),
        "params": params,
    }

    return _kwargs


def _parse_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Any | None:
    if response.status_code == 101:
        return None

    if response.status_code == 401:
        return None

    if response.status_code == 429:
        return None

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Response[Any]:
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
    token: str,
) -> Response[Any]:
    """Attach to PTY session (WebSocket)

     Upgrades to a WebSocket bridged to the guest PTY. Authenticates with the `token` query parameter
    carrying the one-time `connect_token` from the create-session response, not the X-API-Key header.
    Tokens expire after 5 minutes or on first successful connect. Global, authenticated-tenant, and VM
    active-connection limits are reserved before upgrade; a capacity rejection does not consume a valid
    token. Binary messages carry raw PTY bytes; text messages are JSON controls (client-to-server
    `{"type":"resize","cols":N,"rows":N}`, server-to-client `{"type":"exit","exit_code":N}`).

    Args:
        id (UUID):
        pty_id (UUID):
        token (str):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any]
    """

    kwargs = _get_kwargs(
        id=id,
        pty_id=pty_id,
        token=token,
    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)


async def asyncio_detailed(
    id: UUID,
    pty_id: UUID,
    *,
    client: AuthenticatedClient | Client,
    token: str,
) -> Response[Any]:
    """Attach to PTY session (WebSocket)

     Upgrades to a WebSocket bridged to the guest PTY. Authenticates with the `token` query parameter
    carrying the one-time `connect_token` from the create-session response, not the X-API-Key header.
    Tokens expire after 5 minutes or on first successful connect. Global, authenticated-tenant, and VM
    active-connection limits are reserved before upgrade; a capacity rejection does not consume a valid
    token. Binary messages carry raw PTY bytes; text messages are JSON controls (client-to-server
    `{"type":"resize","cols":N,"rows":N}`, server-to-client `{"type":"exit","exit_code":N}`).

    Args:
        id (UUID):
        pty_id (UUID):
        token (str):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any]
    """

    kwargs = _get_kwargs(
        id=id,
        pty_id=pty_id,
        token=token,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)
