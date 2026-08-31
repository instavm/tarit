from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define

from ..models.share_visibility import ShareVisibility, check_share_visibility
from ..types import UNSET, Unset

T = TypeVar("T", bound="CreateShareRequest")


@_attrs_define
class CreateShareRequest:
    """
    Attributes:
        vm_id (UUID):
        guest_port (int):
        visibility (ShareVisibility | Unset): `private` requires a valid share token at the gateway; `public` does not.
    """

    vm_id: UUID
    guest_port: int
    visibility: ShareVisibility | Unset = UNSET

    def to_dict(self) -> dict[str, Any]:
        vm_id = str(self.vm_id)

        guest_port = self.guest_port

        visibility: str | Unset = UNSET
        if not isinstance(self.visibility, Unset):
            visibility = self.visibility

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "vm_id": vm_id,
                "guest_port": guest_port,
            }
        )
        if visibility is not UNSET:
            field_dict["visibility"] = visibility

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        vm_id = UUID(d.pop("vm_id"))

        guest_port = d.pop("guest_port")

        _visibility = d.pop("visibility", UNSET)
        visibility: ShareVisibility | Unset
        if isinstance(_visibility, Unset):
            visibility = UNSET
        else:
            visibility = check_share_visibility(_visibility)

        create_share_request = cls(
            vm_id=vm_id,
            guest_port=guest_port,
            visibility=visibility,
        )

        return create_share_request
