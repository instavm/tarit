from http import HTTPStatus
from typing import Any, cast
from uuid import UUID

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.audit_event import AuditEvent
from ...types import UNSET, Response, Unset


def _get_kwargs(
    *,
    api_key_id: str | Unset = UNSET,
    vm_id: UUID | Unset = UNSET,
    limit: int | Unset = 100,
) -> dict[str, Any]:

    params: dict[str, Any] = {}

    params["api_key_id"] = api_key_id

    json_vm_id: str | Unset = UNSET
    if not isinstance(vm_id, Unset):
        json_vm_id = str(vm_id)
    params["vm_id"] = json_vm_id

    params["limit"] = limit

    params = {k: v for k, v in params.items() if v is not UNSET and v is not None}

    _kwargs: dict[str, Any] = {
        "method": "get",
        "url": "/v1/audit",
        "params": params,
    }

    return _kwargs


def _parse_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Any | list[AuditEvent] | None:
    if response.status_code == 200:
        response_200 = []
        _response_200 = response.json()
        for response_200_item_data in _response_200:
            response_200_item = AuditEvent.from_dict(response_200_item_data)

            response_200.append(response_200_item)

        return response_200

    if response.status_code == 401:
        response_401 = cast(Any, None)
        return response_401

    if response.status_code == 500:
        response_500 = cast(Any, None)
        return response_500

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[Any | list[AuditEvent]]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: AuthenticatedClient | Client,
    api_key_id: str | Unset = UNSET,
    vm_id: UUID | Unset = UNSET,
    limit: int | Unset = 100,
) -> Response[Any | list[AuditEvent]]:
    """Recent audit trail

     Audited actions from the primary store, newest first; requires a fleet database
    (TARIT_DATABASE_URL). Admins see every key; a non-admin key sees only its own actions.

    Args:
        api_key_id (str | Unset):
        vm_id (UUID | Unset):
        limit (int | Unset):  Default: 100.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | list[AuditEvent]]
    """

    kwargs = _get_kwargs(
        api_key_id=api_key_id,
        vm_id=vm_id,
        limit=limit,
    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)


def sync(
    *,
    client: AuthenticatedClient | Client,
    api_key_id: str | Unset = UNSET,
    vm_id: UUID | Unset = UNSET,
    limit: int | Unset = 100,
) -> Any | list[AuditEvent] | None:
    """Recent audit trail

     Audited actions from the primary store, newest first; requires a fleet database
    (TARIT_DATABASE_URL). Admins see every key; a non-admin key sees only its own actions.

    Args:
        api_key_id (str | Unset):
        vm_id (UUID | Unset):
        limit (int | Unset):  Default: 100.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | list[AuditEvent]
    """

    return sync_detailed(
        client=client,
        api_key_id=api_key_id,
        vm_id=vm_id,
        limit=limit,
    ).parsed


async def asyncio_detailed(
    *,
    client: AuthenticatedClient | Client,
    api_key_id: str | Unset = UNSET,
    vm_id: UUID | Unset = UNSET,
    limit: int | Unset = 100,
) -> Response[Any | list[AuditEvent]]:
    """Recent audit trail

     Audited actions from the primary store, newest first; requires a fleet database
    (TARIT_DATABASE_URL). Admins see every key; a non-admin key sees only its own actions.

    Args:
        api_key_id (str | Unset):
        vm_id (UUID | Unset):
        limit (int | Unset):  Default: 100.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | list[AuditEvent]]
    """

    kwargs = _get_kwargs(
        api_key_id=api_key_id,
        vm_id=vm_id,
        limit=limit,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    *,
    client: AuthenticatedClient | Client,
    api_key_id: str | Unset = UNSET,
    vm_id: UUID | Unset = UNSET,
    limit: int | Unset = 100,
) -> Any | list[AuditEvent] | None:
    """Recent audit trail

     Audited actions from the primary store, newest first; requires a fleet database
    (TARIT_DATABASE_URL). Admins see every key; a non-admin key sees only its own actions.

    Args:
        api_key_id (str | Unset):
        vm_id (UUID | Unset):
        limit (int | Unset):  Default: 100.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | list[AuditEvent]
    """

    return (
        await asyncio_detailed(
            client=client,
            api_key_id=api_key_id,
            vm_id=vm_id,
            limit=limit,
        )
    ).parsed
