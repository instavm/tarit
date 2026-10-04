from http import HTTPStatus
from typing import Any, cast

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.create_ssh_key_body import CreateSshKeyBody
from ...models.ssh_key_response import SshKeyResponse
from ...types import Response


def _get_kwargs(
    *,
    body: CreateSshKeyBody,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/ssh-keys",
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Any | SshKeyResponse | None:
    if response.status_code == 201:
        response_201 = SshKeyResponse.from_dict(response.json())

        return response_201

    if response.status_code == 400:
        response_400 = cast(Any, None)
        return response_400

    if response.status_code == 401:
        response_401 = cast(Any, None)
        return response_401

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[Any | SshKeyResponse]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: AuthenticatedClient | Client,
    body: CreateSshKeyBody,
) -> Response[Any | SshKeyResponse]:
    """Register SSH public key

     Registers an OpenSSH public key for the caller's tenant. RSA keys remain valid for guest authorized-
    key injection but cannot authenticate to the SSH gateway.

    Args:
        body (CreateSshKeyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | SshKeyResponse]
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
    body: CreateSshKeyBody,
) -> Any | SshKeyResponse | None:
    """Register SSH public key

     Registers an OpenSSH public key for the caller's tenant. RSA keys remain valid for guest authorized-
    key injection but cannot authenticate to the SSH gateway.

    Args:
        body (CreateSshKeyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | SshKeyResponse
    """

    return sync_detailed(
        client=client,
        body=body,
    ).parsed


async def asyncio_detailed(
    *,
    client: AuthenticatedClient | Client,
    body: CreateSshKeyBody,
) -> Response[Any | SshKeyResponse]:
    """Register SSH public key

     Registers an OpenSSH public key for the caller's tenant. RSA keys remain valid for guest authorized-
    key injection but cannot authenticate to the SSH gateway.

    Args:
        body (CreateSshKeyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | SshKeyResponse]
    """

    kwargs = _get_kwargs(
        body=body,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    *,
    client: AuthenticatedClient | Client,
    body: CreateSshKeyBody,
) -> Any | SshKeyResponse | None:
    """Register SSH public key

     Registers an OpenSSH public key for the caller's tenant. RSA keys remain valid for guest authorized-
    key injection but cannot authenticate to the SSH gateway.

    Args:
        body (CreateSshKeyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | SshKeyResponse
    """

    return (
        await asyncio_detailed(
            client=client,
            body=body,
        )
    ).parsed
