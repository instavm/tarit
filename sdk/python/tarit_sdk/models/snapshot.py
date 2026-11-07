from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define

from ..types import UNSET, Unset

T = TypeVar("T", bound="Snapshot")


@_attrs_define
class Snapshot:
    """
    Attributes:
        snapshot_id (UUID):
        source_vm_id (UUID):
        created_at (datetime.datetime):
        size_bytes (int | Unset):
        memory_mib (int | Unset):
        vcpus (int | Unset):
    """

    snapshot_id: UUID
    source_vm_id: UUID
    created_at: datetime.datetime
    size_bytes: int | Unset = UNSET
    memory_mib: int | Unset = UNSET
    vcpus: int | Unset = UNSET

    def to_dict(self) -> dict[str, Any]:
        snapshot_id = str(self.snapshot_id)

        source_vm_id = str(self.source_vm_id)

        created_at = self.created_at.isoformat()

        size_bytes = self.size_bytes

        memory_mib = self.memory_mib

        vcpus = self.vcpus

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "snapshot_id": snapshot_id,
                "source_vm_id": source_vm_id,
                "created_at": created_at,
            }
        )
        if size_bytes is not UNSET:
            field_dict["size_bytes"] = size_bytes
        if memory_mib is not UNSET:
            field_dict["memory_mib"] = memory_mib
        if vcpus is not UNSET:
            field_dict["vcpus"] = vcpus

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        snapshot_id = UUID(d.pop("snapshot_id"))

        source_vm_id = UUID(d.pop("source_vm_id"))

        created_at = datetime.datetime.fromisoformat(d.pop("created_at"))

        size_bytes = d.pop("size_bytes", UNSET)

        memory_mib = d.pop("memory_mib", UNSET)

        vcpus = d.pop("vcpus", UNSET)

        snapshot = cls(
            snapshot_id=snapshot_id,
            source_vm_id=source_vm_id,
            created_at=created_at,
            size_bytes=size_bytes,
            memory_mib=memory_mib,
            vcpus=vcpus,
        )

        return snapshot
