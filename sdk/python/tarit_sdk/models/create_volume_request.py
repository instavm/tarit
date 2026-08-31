from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define

from ..models.create_volume_request_provider import CreateVolumeRequestProvider, check_create_volume_request_provider
from ..types import UNSET, Unset

T = TypeVar("T", bound="CreateVolumeRequest")


@_attrs_define
class CreateVolumeRequest:
    """
    Attributes:
        name (str):
        size_bytes (int):
        id (UUID | Unset): Optional client-selected identity for idempotent replay.
        provider (CreateVolumeRequestProvider | Unset):  Default: 'local_block'.
    """

    name: str
    size_bytes: int
    id: UUID | Unset = UNSET
    provider: CreateVolumeRequestProvider | Unset = "local_block"

    def to_dict(self) -> dict[str, Any]:
        name = self.name

        size_bytes = self.size_bytes

        id: str | Unset = UNSET
        if not isinstance(self.id, Unset):
            id = str(self.id)

        provider: str | Unset = UNSET
        if not isinstance(self.provider, Unset):
            provider = self.provider

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "name": name,
                "size_bytes": size_bytes,
            }
        )
        if id is not UNSET:
            field_dict["id"] = id
        if provider is not UNSET:
            field_dict["provider"] = provider

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        name = d.pop("name")

        size_bytes = d.pop("size_bytes")

        _id = d.pop("id", UNSET)
        id: UUID | Unset
        if isinstance(_id, Unset):
            id = UNSET
        else:
            id = UUID(_id)

        _provider = d.pop("provider", UNSET)
        provider: CreateVolumeRequestProvider | Unset
        if isinstance(_provider, Unset):
            provider = UNSET
        else:
            provider = check_create_volume_request_provider(_provider)

        create_volume_request = cls(
            name=name,
            size_bytes=size_bytes,
            id=id,
            provider=provider,
        )

        return create_volume_request
