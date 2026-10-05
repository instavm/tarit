from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define

from ..types import UNSET, Unset

T = TypeVar("T", bound="RestoreRequest")


@_attrs_define
class RestoreRequest:
    """
    Attributes:
        snapshot_id (UUID): Opaque snapshot handle. Physical storage paths and host placement are never public.
        target_memory_mib (int | Unset): Grow guest-visible RAM to this total within the snapshot reserved maximum.
            Requires a live hotplug-ready snapshot; shrinking is rejected.
        id (UUID | Unset): Optional id for the restored VM; generated when omitted.
    """

    snapshot_id: UUID
    target_memory_mib: int | Unset = UNSET
    id: UUID | Unset = UNSET

    def to_dict(self) -> dict[str, Any]:
        snapshot_id = str(self.snapshot_id)

        target_memory_mib = self.target_memory_mib

        id: str | Unset = UNSET
        if not isinstance(self.id, Unset):
            id = str(self.id)

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "snapshot_id": snapshot_id,
            }
        )
        if target_memory_mib is not UNSET:
            field_dict["target_memory_mib"] = target_memory_mib
        if id is not UNSET:
            field_dict["id"] = id

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        snapshot_id = UUID(d.pop("snapshot_id"))

        target_memory_mib = d.pop("target_memory_mib", UNSET)

        _id = d.pop("id", UNSET)
        id: UUID | Unset
        if isinstance(_id, Unset):
            id = UNSET
        else:
            id = UUID(_id)

        restore_request = cls(
            snapshot_id=snapshot_id,
            target_memory_mib=target_memory_mib,
            id=id,
        )

        return restore_request
