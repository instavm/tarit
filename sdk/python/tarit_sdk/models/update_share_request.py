from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define

from ..models.share_visibility import ShareVisibility, check_share_visibility
from ..types import UNSET, Unset

T = TypeVar("T", bound="UpdateShareRequest")


@_attrs_define
class UpdateShareRequest:
    """
    Attributes:
        vm_id (UUID | Unset):
        guest_port (int | Unset):
        visibility (ShareVisibility | Unset): `private` requires a valid share token at the gateway; `public` does not.
    """

    vm_id: UUID | Unset = UNSET
    guest_port: int | Unset = UNSET
    visibility: ShareVisibility | Unset = UNSET

    def to_dict(self) -> dict[str, Any]:
        vm_id: str | Unset = UNSET
        if not isinstance(self.vm_id, Unset):
            vm_id = str(self.vm_id)

        guest_port = self.guest_port

        visibility: str | Unset = UNSET
        if not isinstance(self.visibility, Unset):
            visibility = self.visibility

        field_dict: dict[str, Any] = {}

        field_dict.update({})
        if vm_id is not UNSET:
            field_dict["vm_id"] = vm_id
        if guest_port is not UNSET:
            field_dict["guest_port"] = guest_port
        if visibility is not UNSET:
            field_dict["visibility"] = visibility

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        _vm_id = d.pop("vm_id", UNSET)
        vm_id: UUID | Unset
        if isinstance(_vm_id, Unset):
            vm_id = UNSET
        else:
            vm_id = UUID(_vm_id)

        guest_port = d.pop("guest_port", UNSET)

        _visibility = d.pop("visibility", UNSET)
        visibility: ShareVisibility | Unset
        if isinstance(_visibility, Unset):
            visibility = UNSET
        else:
            visibility = check_share_visibility(_visibility)

        update_share_request = cls(
            vm_id=vm_id,
            guest_port=guest_port,
            visibility=visibility,
        )

        return update_share_request
