from http import HTTPStatus
from typing import Any, cast

import httpx

from ... import errors
from ...client import AuthenticatedClient, Client
from ...models.execute_request import ExecuteRequest
from ...models.execution_record import ExecutionRecord
from ...types import Response


def _get_kwargs(
    *,
    body: ExecuteRequest,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/execute_async",
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(*, client: AuthenticatedClient | Client, response: httpx.Response) -> Any | ExecutionRecord | None:
    if response.status_code == 202:
        response_202 = ExecutionRecord.from_dict(response.json())

        return response_202

    if response.status_code == 401:
        response_401 = cast(Any, None)
        return response_401

    if response.status_code == 403:
        response_403 = cast(Any, None)
        return response_403

    if response.status_code == 404:
        response_404 = cast(Any, None)
        return response_404

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[Any | ExecutionRecord]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: AuthenticatedClient | Client,
    body: ExecuteRequest,
) -> Response[Any | ExecutionRecord]:
    """Execute command in sandbox (async)

     User keys can execute only in their tenant's VMs; admin keys can execute in any VM. Poll the record
    with GET /v1/executions/{id} on the same node.

    Args:
        body (ExecuteRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | ExecutionRecord]
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
    body: ExecuteRequest,
) -> Any | ExecutionRecord | None:
    """Execute command in sandbox (async)

     User keys can execute only in their tenant's VMs; admin keys can execute in any VM. Poll the record
    with GET /v1/executions/{id} on the same node.

    Args:
        body (ExecuteRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | ExecutionRecord
    """

    return sync_detailed(
        client=client,
        body=body,
    ).parsed


async def asyncio_detailed(
    *,
    client: AuthenticatedClient | Client,
    body: ExecuteRequest,
) -> Response[Any | ExecutionRecord]:
    """Execute command in sandbox (async)

     User keys can execute only in their tenant's VMs; admin keys can execute in any VM. Poll the record
    with GET /v1/executions/{id} on the same node.

    Args:
        body (ExecuteRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Any | ExecutionRecord]
    """

    kwargs = _get_kwargs(
        body=body,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    *,
    client: AuthenticatedClient | Client,
    body: ExecuteRequest,
) -> Any | ExecutionRecord | None:
    """Execute command in sandbox (async)

     User keys can execute only in their tenant's VMs; admin keys can execute in any VM. Poll the record
    with GET /v1/executions/{id} on the same node.

    Args:
        body (ExecuteRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Any | ExecutionRecord
    """

    return (
        await asyncio_detailed(
            client=client,
            body=body,
        )
    ).parsed
