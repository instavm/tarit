from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

T = TypeVar("T", bound="UpdateBranchBody")


@_attrs_define
class UpdateBranchBody:
    """
    Attributes:
        expected_revision (int):
        head_artifact_id (UUID):
    """

    expected_revision: int
    head_artifact_id: UUID
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        expected_revision = self.expected_revision

        head_artifact_id = str(self.head_artifact_id)

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "expected_revision": expected_revision,
                "head_artifact_id": head_artifact_id,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        expected_revision = d.pop("expected_revision")

        head_artifact_id = UUID(d.pop("head_artifact_id"))

        update_branch_body = cls(
            expected_revision=expected_revision,
            head_artifact_id=head_artifact_id,
        )

        update_branch_body.additional_properties = d
        return update_branch_body

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
