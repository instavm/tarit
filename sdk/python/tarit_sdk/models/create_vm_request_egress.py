from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, cast

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

T = TypeVar("T", bound="CreateVmRequestEgress")


@_attrs_define
class CreateVmRequestEgress:
    """Egress policy applied as the VM is created, in the same form as PATCH /v1/egress/vm/{id}. Without one, a networked
    VM can reach nothing. Invalid policies are rejected before anything is created; if the policy cannot be applied, the
    VM is removed and the create fails.

        Attributes:
            allowlist (list[str]):
            allow_existing (bool | Unset):  Default: False.
    """

    allowlist: list[str]
    allow_existing: bool | Unset = False
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        allowlist = self.allowlist

        allow_existing = self.allow_existing

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "allowlist": allowlist,
            }
        )
        if allow_existing is not UNSET:
            field_dict["allow_existing"] = allow_existing

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        allowlist = cast(list[str], d.pop("allowlist"))

        allow_existing = d.pop("allow_existing", UNSET)

        create_vm_request_egress = cls(
            allowlist=allowlist,
            allow_existing=allow_existing,
        )

        create_vm_request_egress.additional_properties = d
        return create_vm_request_egress

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
