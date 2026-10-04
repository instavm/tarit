from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

T = TypeVar("T", bound="Branch")


@_attrs_define
class Branch:
    """
    Attributes:
        branch_id (UUID):
        name (str):
        head_artifact_id (UUID):
        revision (int):
        created_at (datetime.datetime):
        updated_at (datetime.datetime):
        source_vm_id (UUID | Unset):
        source_branch_id (UUID | Unset):
    """

    branch_id: UUID
    name: str
    head_artifact_id: UUID
    revision: int
    created_at: datetime.datetime
    updated_at: datetime.datetime
    source_vm_id: UUID | Unset = UNSET
    source_branch_id: UUID | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        branch_id = str(self.branch_id)

        name = self.name

        head_artifact_id = str(self.head_artifact_id)

        revision = self.revision

        created_at = self.created_at.isoformat()

        updated_at = self.updated_at.isoformat()

        source_vm_id: str | Unset = UNSET
        if not isinstance(self.source_vm_id, Unset):
            source_vm_id = str(self.source_vm_id)

        source_branch_id: str | Unset = UNSET
        if not isinstance(self.source_branch_id, Unset):
            source_branch_id = str(self.source_branch_id)

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "branch_id": branch_id,
                "name": name,
                "head_artifact_id": head_artifact_id,
                "revision": revision,
                "created_at": created_at,
                "updated_at": updated_at,
            }
        )
        if source_vm_id is not UNSET:
            field_dict["source_vm_id"] = source_vm_id
        if source_branch_id is not UNSET:
            field_dict["source_branch_id"] = source_branch_id

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        branch_id = UUID(d.pop("branch_id"))

        name = d.pop("name")

        head_artifact_id = UUID(d.pop("head_artifact_id"))

        revision = d.pop("revision")

        created_at = datetime.datetime.fromisoformat(d.pop("created_at"))

        updated_at = datetime.datetime.fromisoformat(d.pop("updated_at"))

        _source_vm_id = d.pop("source_vm_id", UNSET)
        source_vm_id: UUID | Unset
        if isinstance(_source_vm_id, Unset):
            source_vm_id = UNSET
        else:
            source_vm_id = UUID(_source_vm_id)

        _source_branch_id = d.pop("source_branch_id", UNSET)
        source_branch_id: UUID | Unset
        if isinstance(_source_branch_id, Unset):
            source_branch_id = UNSET
        else:
            source_branch_id = UUID(_source_branch_id)

        branch = cls(
            branch_id=branch_id,
            name=name,
            head_artifact_id=head_artifact_id,
            revision=revision,
            created_at=created_at,
            updated_at=updated_at,
            source_vm_id=source_vm_id,
            source_branch_id=source_branch_id,
        )

        branch.additional_properties = d
        return branch

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
