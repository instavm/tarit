from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote
from uuid import UUID

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.resize_pty_session_body import ResizePtySessionBody
from ...models.resize_pty_session_response_200 import ResizePtySessionResponse200
from ...types import Response


def _get_kwargs(
    id: UUID,
    pty_id: UUID,
    *,
    body: ResizePtySessionBody,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/vms/{id}/pty/sessions/{pty_id}/resize".format(
            id=quote(str(id), safe=""),
            pty_id=quote(str(pty_id), safe=""),
        ),
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Any | ResizePtySessionResponse200 | None:
    if response.status_code == 200:
        response_200 = ResizePtySessionResponse200.from_dict(response.json())

        return response_200

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

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[Any | ResizePtySessionResponse200]:
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
    body: ResizePtySessionBody,
) -> Response[Any | ResizePtySessionResponse200]:
    """Resize PTY session

    Args:
        id (UUID):
        pty_id (UUID):
        body (ResizePtySessionBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | ResizePtySessionResponse200]
    """

    kwargs = _get_kwargs(
        id=id,
        pty_id=pty_id,
        body=body,
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
    body: ResizePtySessionBody,
) -> Any | ResizePtySessionResponse200 | None:
    """Resize PTY session

    Args:
        id (UUID):
        pty_id (UUID):
        body (ResizePtySessionBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | ResizePtySessionResponse200
    """

    return sync_detailed(
        id=id,
        pty_id=pty_id,
        client=client,
        body=body,
    ).parsed


async def asyncio_detailed(
    id: UUID,
    pty_id: UUID,
    *,
    client: AuthenticatedClient | Client,
    body: ResizePtySessionBody,
) -> Response[Any | ResizePtySessionResponse200]:
    """Resize PTY session

    Args:
        id (UUID):
        pty_id (UUID):
        body (ResizePtySessionBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | ResizePtySessionResponse200]
    """

    kwargs = _get_kwargs(
        id=id,
        pty_id=pty_id,
        body=body,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    id: UUID,
    pty_id: UUID,
    *,
    client: AuthenticatedClient | Client,
    body: ResizePtySessionBody,
) -> Any | ResizePtySessionResponse200 | None:
    """Resize PTY session

    Args:
        id (UUID):
        pty_id (UUID):
        body (ResizePtySessionBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | ResizePtySessionResponse200
    """

    return (
        await asyncio_detailed(
            id=id,
            pty_id=pty_id,
            client=client,
            body=body,
        )
    ).parsed
