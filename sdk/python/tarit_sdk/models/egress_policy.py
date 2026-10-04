from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import Any, TypeVar, cast
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

T = TypeVar("T", bound="EgressPolicy")


@_attrs_define
class EgressPolicy:
    """
    Attributes:
        vm_id (UUID):
        revision (int):
        allowlist (list[str]):
        allow_existing (bool):
        created_at (datetime.datetime):
        updated_at (datetime.datetime):
    """

    vm_id: UUID
    revision: int
    allowlist: list[str]
    allow_existing: bool
    created_at: datetime.datetime
    updated_at: datetime.datetime
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        vm_id = str(self.vm_id)

        revision = self.revision

        allowlist = self.allowlist

        allow_existing = self.allow_existing

        created_at = self.created_at.isoformat()

        updated_at = self.updated_at.isoformat()

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "vm_id": vm_id,
                "revision": revision,
                "allowlist": allowlist,
                "allow_existing": allow_existing,
                "created_at": created_at,
                "updated_at": updated_at,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        vm_id = UUID(d.pop("vm_id"))

        revision = d.pop("revision")

        allowlist = cast(list[str], d.pop("allowlist"))

        allow_existing = d.pop("allow_existing")

        created_at = datetime.datetime.fromisoformat(d.pop("created_at"))

        updated_at = datetime.datetime.fromisoformat(d.pop("updated_at"))

        egress_policy = cls(
            vm_id=vm_id,
            revision=revision,
            allowlist=allowlist,
            allow_existing=allow_existing,
            created_at=created_at,
            updated_at=updated_at,
        )

        egress_policy.additional_properties = d
        return egress_policy

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
