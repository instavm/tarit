from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote
from uuid import UUID

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.patch_vm_egress_policy_body import PatchVmEgressPolicyBody
from ...models.patch_vm_egress_policy_response_200 import PatchVmEgressPolicyResponse200
from ...types import Response


def _get_kwargs(
    id: UUID,
    *,
    body: PatchVmEgressPolicyBody,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "patch",
        "url": "/v1/egress/vm/{id}".format(
            id=quote(str(id), safe=""),
        ),
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Any | PatchVmEgressPolicyResponse200 | None:
    if response.status_code == 200:
        response_200 = PatchVmEgressPolicyResponse200.from_dict(response.json())

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
) -> Response[Any | PatchVmEgressPolicyResponse200]:
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
    body: PatchVmEgressPolicyBody,
) -> Response[Any | PatchVmEgressPolicyResponse200]:
    """Update egress allowlist

     User keys can update egress only for their tenant's VMs; admin keys can update any VM.

    Args:
        id (UUID):
        body (PatchVmEgressPolicyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | PatchVmEgressPolicyResponse200]
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
    body: PatchVmEgressPolicyBody,
) -> Any | PatchVmEgressPolicyResponse200 | None:
    """Update egress allowlist

     User keys can update egress only for their tenant's VMs; admin keys can update any VM.

    Args:
        id (UUID):
        body (PatchVmEgressPolicyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | PatchVmEgressPolicyResponse200
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
    body: PatchVmEgressPolicyBody,
) -> Response[Any | PatchVmEgressPolicyResponse200]:
    """Update egress allowlist

     User keys can update egress only for their tenant's VMs; admin keys can update any VM.

    Args:
        id (UUID):
        body (PatchVmEgressPolicyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | PatchVmEgressPolicyResponse200]
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
    body: PatchVmEgressPolicyBody,
) -> Any | PatchVmEgressPolicyResponse200 | None:
    """Update egress allowlist

     User keys can update egress only for their tenant's VMs; admin keys can update any VM.

    Args:
        id (UUID):
        body (PatchVmEgressPolicyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | PatchVmEgressPolicyResponse200
    """

    return (
        await asyncio_detailed(
            id=id,
            client=client,
            body=body,
        )
    ).parsed
