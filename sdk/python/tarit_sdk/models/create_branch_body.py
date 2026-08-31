from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

T = TypeVar("T", bound="CreateBranchBody")


@_attrs_define
class CreateBranchBody:
    """
    Attributes:
        name (str):
        head_artifact_id (UUID):
        branch_id (UUID | Unset):
        source_vm_id (UUID | Unset):
        source_branch_id (UUID | Unset):
    """

    name: str
    head_artifact_id: UUID
    branch_id: UUID | Unset = UNSET
    source_vm_id: UUID | Unset = UNSET
    source_branch_id: UUID | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        name = self.name

        head_artifact_id = str(self.head_artifact_id)

        branch_id: str | Unset = UNSET
        if not isinstance(self.branch_id, Unset):
            branch_id = str(self.branch_id)

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
                "name": name,
                "head_artifact_id": head_artifact_id,
            }
        )
        if branch_id is not UNSET:
            field_dict["branch_id"] = branch_id
        if source_vm_id is not UNSET:
            field_dict["source_vm_id"] = source_vm_id
        if source_branch_id is not UNSET:
            field_dict["source_branch_id"] = source_branch_id

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        name = d.pop("name")

        head_artifact_id = UUID(d.pop("head_artifact_id"))

        _branch_id = d.pop("branch_id", UNSET)
        branch_id: UUID | Unset
        if isinstance(_branch_id, Unset):
            branch_id = UNSET
        else:
            branch_id = UUID(_branch_id)

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

        create_branch_body = cls(
            name=name,
            head_artifact_id=head_artifact_id,
            branch_id=branch_id,
            source_vm_id=source_vm_id,
            source_branch_id=source_branch_id,
        )

        create_branch_body.additional_properties = d
        return create_branch_body

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
