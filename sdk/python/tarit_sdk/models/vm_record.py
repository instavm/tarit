from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..models.vm_record_startup_path import VmRecordStartupPath, check_vm_record_startup_path
from ..models.vm_record_status import VmRecordStatus, check_vm_record_status
from ..types import UNSET, Unset

T = TypeVar("T", bound="VmRecord")


@_attrs_define
class VmRecord:
    """Tenant-safe VM record. Host identity, filesystem paths, VMM socket, process id, boot arguments, and ownership
    metadata are never exposed.

        Attributes:
            id (UUID):
            status (VmRecordStatus):
            revision (int):
            memory_mib (int):
            vcpus (int):
            created_at (datetime.datetime):
            updated_at (datetime.datetime):
            startup_path (VmRecordStartupPath | Unset): Definitive lifecycle branch used to start this VM. Absent only on
                legacy records.
    """

    id: UUID
    status: VmRecordStatus
    revision: int
    memory_mib: int
    vcpus: int
    created_at: datetime.datetime
    updated_at: datetime.datetime
    startup_path: VmRecordStartupPath | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        id = str(self.id)

        status: str = self.status

        revision = self.revision

        memory_mib = self.memory_mib

        vcpus = self.vcpus

        created_at = self.created_at.isoformat()

        updated_at = self.updated_at.isoformat()

        startup_path: str | Unset = UNSET
        if not isinstance(self.startup_path, Unset):
            startup_path = self.startup_path

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "id": id,
                "status": status,
                "revision": revision,
                "memory_mib": memory_mib,
                "vcpus": vcpus,
                "created_at": created_at,
                "updated_at": updated_at,
            }
        )
        if startup_path is not UNSET:
            field_dict["startup_path"] = startup_path

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        id = UUID(d.pop("id"))

        status = check_vm_record_status(d.pop("status"))

        revision = d.pop("revision")

        memory_mib = d.pop("memory_mib")

        vcpus = d.pop("vcpus")

        created_at = datetime.datetime.fromisoformat(d.pop("created_at"))

        updated_at = datetime.datetime.fromisoformat(d.pop("updated_at"))

        _startup_path = d.pop("startup_path", UNSET)
        startup_path: VmRecordStartupPath | Unset
        if isinstance(_startup_path, Unset):
            startup_path = UNSET
        else:
            startup_path = check_vm_record_startup_path(_startup_path)

        vm_record = cls(
            id=id,
            status=status,
            revision=revision,
            memory_mib=memory_mib,
            vcpus=vcpus,
            created_at=created_at,
            updated_at=updated_at,
            startup_path=startup_path,
        )

        vm_record.additional_properties = d
        return vm_record

    @property
    def additional_keys(self) -> list[str]:
        return list(self.additional_properties.keys())

    def __getitem__(self, key: str) -> Any:
        return self.additional_properties[key]

    def __setitem__(self, key: str, value: Any) -> None:
        self.additional_properties[key] = value

    def __delitem__(self, key: str) -> None:
        del self.additional_properties[key]

    def __contains__(self, key: str) -> bool:
        return key in self.additional_properties
