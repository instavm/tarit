import datetime
from http import HTTPStatus
from typing import Any, cast

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.usage_summary import UsageSummary
from ...types import UNSET, Response, Unset


def _get_kwargs(
    *,
    from_: datetime.datetime | Unset = UNSET,
    to: datetime.datetime | Unset = UNSET,
    api_key_id: str | Unset = UNSET,
) -> dict[str, Any]:

    params: dict[str, Any] = {}

    json_from_: str | Unset = UNSET
    if not isinstance(from_, Unset):
        json_from_ = from_.isoformat()
    params["from"] = json_from_

    json_to: str | Unset = UNSET
    if not isinstance(to, Unset):
        json_to = to.isoformat()
    params["to"] = json_to

    params["api_key_id"] = api_key_id

    params = {k: v for k, v in params.items() if v is not UNSET and v is not None}

    _kwargs: dict[str, Any] = {
        "method": "get",
        "url": "/v1/usage",
        "params": params,
    }

    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Any | list[UsageSummary] | None:
    if response.status_code == 200:
        response_200 = []
        _response_200 = response.json()
        for response_200_item_data in _response_200:
            response_200_item = UsageSummary.from_dict(response_200_item_data)

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
) -> Response[Any | list[UsageSummary]]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: AuthenticatedClient | Client,
    from_: datetime.datetime | Unset = UNSET,
    to: datetime.datetime | Unset = UNSET,
    api_key_id: str | Unset = UNSET,
) -> Response[Any | list[UsageSummary]]:
    """Aggregated usage stats per API key

     Raw metering (VM runtime seconds, exec counts and durations) from the primary store; requires a
    fleet database (TARIT_DATABASE_URL). Admins see every key, optionally filtered by api_key_id; a non-
    admin key sees only its own.

    Args:
        from_ (datetime.datetime | Unset):
        to (datetime.datetime | Unset):
        api_key_id (str | Unset):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | list[UsageSummary]]
    """

    kwargs = _get_kwargs(
        from_=from_,
        to=to,
        api_key_id=api_key_id,
    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)


def sync(
    *,
    client: AuthenticatedClient | Client,
    from_: datetime.datetime | Unset = UNSET,
    to: datetime.datetime | Unset = UNSET,
    api_key_id: str | Unset = UNSET,
) -> Any | list[UsageSummary] | None:
    """Aggregated usage stats per API key

     Raw metering (VM runtime seconds, exec counts and durations) from the primary store; requires a
    fleet database (TARIT_DATABASE_URL). Admins see every key, optionally filtered by api_key_id; a non-
    admin key sees only its own.

    Args:
        from_ (datetime.datetime | Unset):
        to (datetime.datetime | Unset):
        api_key_id (str | Unset):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | list[UsageSummary]
    """

    return sync_detailed(
        client=client,
        from_=from_,
        to=to,
        api_key_id=api_key_id,
    ).parsed


async def asyncio_detailed(
    *,
    client: AuthenticatedClient | Client,
    from_: datetime.datetime | Unset = UNSET,
    to: datetime.datetime | Unset = UNSET,
    api_key_id: str | Unset = UNSET,
) -> Response[Any | list[UsageSummary]]:
    """Aggregated usage stats per API key

     Raw metering (VM runtime seconds, exec counts and durations) from the primary store; requires a
    fleet database (TARIT_DATABASE_URL). Admins see every key, optionally filtered by api_key_id; a non-
    admin key sees only its own.

    Args:
        from_ (datetime.datetime | Unset):
        to (datetime.datetime | Unset):
        api_key_id (str | Unset):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | list[UsageSummary]]
    """

    kwargs = _get_kwargs(
        from_=from_,
        to=to,
        api_key_id=api_key_id,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    *,
    client: AuthenticatedClient | Client,
    from_: datetime.datetime | Unset = UNSET,
    to: datetime.datetime | Unset = UNSET,
    api_key_id: str | Unset = UNSET,
) -> Any | list[UsageSummary] | None:
    """Aggregated usage stats per API key

     Raw metering (VM runtime seconds, exec counts and durations) from the primary store; requires a
    fleet database (TARIT_DATABASE_URL). Admins see every key, optionally filtered by api_key_id; a non-
    admin key sees only its own.

    Args:
        from_ (datetime.datetime | Unset):
        to (datetime.datetime | Unset):
        api_key_id (str | Unset):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | list[UsageSummary]
    """

    return (
        await asyncio_detailed(
            client=client,
            from_=from_,
            to=to,
            api_key_id=api_key_id,
        )
    ).parsed
