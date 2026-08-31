from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, cast

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

T = TypeVar("T", bound="SetVmEgressPolicyBody")


@_attrs_define
class SetVmEgressPolicyBody:
    """
    Attributes:
        expected_revision (int):
        allowlist (list[str]):
        allow_existing (bool | Unset):  Default: False.
    """

    expected_revision: int
    allowlist: list[str]
    allow_existing: bool | Unset = False
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        expected_revision = self.expected_revision

        allowlist = self.allowlist

        allow_existing = self.allow_existing

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "expected_revision": expected_revision,
                "allowlist": allowlist,
            }
        )
        if allow_existing is not UNSET:
            field_dict["allow_existing"] = allow_existing

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        expected_revision = d.pop("expected_revision")

        allowlist = cast(list[str], d.pop("allowlist"))

        allow_existing = d.pop("allow_existing", UNSET)

        set_vm_egress_policy_body = cls(
            expected_revision=expected_revision,
            allowlist=allowlist,
            allow_existing=allow_existing,
        )

        set_vm_egress_policy_body.additional_properties = d
        return set_vm_egress_policy_body

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
