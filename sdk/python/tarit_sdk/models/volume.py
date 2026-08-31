from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, TypeVar, cast
from uuid import UUID

from attrs import define as _attrs_define

from ..models.volume_provider import VolumeProvider, check_volume_provider
from ..models.volume_status import VolumeStatus, check_volume_status
from ..models.volume_storage_class import VolumeStorageClass, check_volume_storage_class
from ..types import UNSET, Unset

if TYPE_CHECKING:
    from ..models.volume_capabilities import VolumeCapabilities


T = TypeVar("T", bound="Volume")


@_attrs_define
class Volume:
    """Tenant-safe persistent-volume record. Host identity, device paths, provider locators, credentials, and private
    errors are never exposed.

        Attributes:
            id (UUID):
            name (str):
            provider (VolumeProvider):
            storage_class (VolumeStorageClass):
            size_bytes (int):
            status (VolumeStatus):
            capabilities (VolumeCapabilities):
            generation (int):
            revision (int):
            created_at (datetime.datetime):
            updated_at (datetime.datetime):
            region (None | str | Unset):
            zone (None | str | Unset):
    """

    id: UUID
    name: str
    provider: VolumeProvider
    storage_class: VolumeStorageClass
    size_bytes: int
    status: VolumeStatus
    capabilities: VolumeCapabilities
    generation: int
    revision: int
    created_at: datetime.datetime
    updated_at: datetime.datetime
    region: None | str | Unset = UNSET
    zone: None | str | Unset = UNSET

    def to_dict(self) -> dict[str, Any]:
        id = str(self.id)

        name = self.name

        provider: str = self.provider

        storage_class: str = self.storage_class

        size_bytes = self.size_bytes

        status: str = self.status

        capabilities = self.capabilities.to_dict()

        generation = self.generation

        revision = self.revision

        created_at = self.created_at.isoformat()

        updated_at = self.updated_at.isoformat()

        region: None | str | Unset
        if isinstance(self.region, Unset):
            region = UNSET
        else:
            region = self.region

        zone: None | str | Unset
        if isinstance(self.zone, Unset):
            zone = UNSET
        else:
            zone = self.zone

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "id": id,
                "name": name,
                "provider": provider,
                "storage_class": storage_class,
                "size_bytes": size_bytes,
                "status": status,
                "capabilities": capabilities,
                "generation": generation,
                "revision": revision,
                "created_at": created_at,
                "updated_at": updated_at,
            }
        )
        if region is not UNSET:
            field_dict["region"] = region
        if zone is not UNSET:
            field_dict["zone"] = zone

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.volume_capabilities import VolumeCapabilities  # noqa: PLC0415

        d = dict(src_dict)
        id = UUID(d.pop("id"))

        name = d.pop("name")

        provider = check_volume_provider(d.pop("provider"))

        storage_class = check_volume_storage_class(d.pop("storage_class"))

        size_bytes = d.pop("size_bytes")

        status = check_volume_status(d.pop("status"))

        capabilities = VolumeCapabilities.from_dict(d.pop("capabilities"))

        generation = d.pop("generation")

        revision = d.pop("revision")

        created_at = datetime.datetime.fromisoformat(d.pop("created_at"))

        updated_at = datetime.datetime.fromisoformat(d.pop("updated_at"))

        def _parse_region(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        region = _parse_region(d.pop("region", UNSET))

        def _parse_zone(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        zone = _parse_zone(d.pop("zone", UNSET))

        volume = cls(
            id=id,
            name=name,
            provider=provider,
            storage_class=storage_class,
            size_bytes=size_bytes,
            status=status,
            capabilities=capabilities,
            generation=generation,
            revision=revision,
            created_at=created_at,
            updated_at=updated_at,
            region=region,
            zone=zone,
        )

        return volume
