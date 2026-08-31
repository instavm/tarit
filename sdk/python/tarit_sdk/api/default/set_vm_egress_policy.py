from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote
from uuid import UUID

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.egress_policy import EgressPolicy
from ...models.set_vm_egress_policy_body import SetVmEgressPolicyBody
from ...types import Response


def _get_kwargs(
    id: UUID,
    *,
    body: SetVmEgressPolicyBody,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "put",
        "url": "/v1/vms/{id}/egress-policy".format(
            id=quote(str(id), safe=""),
        ),
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Any | EgressPolicy | None:
    if response.status_code == 200:
        response_200 = EgressPolicy.from_dict(response.json())

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


def _build_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Response[Any | EgressPolicy]:
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
    body: SetVmEgressPolicyBody,
) -> Response[Any | EgressPolicy]:
    """Compare-and-swap durable desired egress policy

     Persists the policy before applying it. For a hibernated VM it is installed during network
    provisioning before the resumed guest starts.

    Args:
        id (UUID):
        body (SetVmEgressPolicyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | EgressPolicy]
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
    body: SetVmEgressPolicyBody,
) -> Any | EgressPolicy | None:
    """Compare-and-swap durable desired egress policy

     Persists the policy before applying it. For a hibernated VM it is installed during network
    provisioning before the resumed guest starts.

    Args:
        id (UUID):
        body (SetVmEgressPolicyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | EgressPolicy
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
    body: SetVmEgressPolicyBody,
) -> Response[Any | EgressPolicy]:
    """Compare-and-swap durable desired egress policy

     Persists the policy before applying it. For a hibernated VM it is installed during network
    provisioning before the resumed guest starts.

    Args:
        id (UUID):
        body (SetVmEgressPolicyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | EgressPolicy]
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
    body: SetVmEgressPolicyBody,
) -> Any | EgressPolicy | None:
    """Compare-and-swap durable desired egress policy

     Persists the policy before applying it. For a hibernated VM it is installed during network
    provisioning before the resumed guest starts.

    Args:
        id (UUID):
        body (SetVmEgressPolicyBody):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | EgressPolicy
    """

    return (
        await asyncio_detailed(
            id=id,
            client=client,
            body=body,
        )
    ).parsed
