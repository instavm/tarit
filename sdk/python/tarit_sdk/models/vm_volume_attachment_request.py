from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define

from ..models.vm_volume_attachment_request_mode import (
    VmVolumeAttachmentRequestMode,
    check_vm_volume_attachment_request_mode,
)

T = TypeVar("T", bound="VmVolumeAttachmentRequest")


@_attrs_define
class VmVolumeAttachmentRequest:
    """
    Attributes:
        volume_id (UUID):
        mode (VmVolumeAttachmentRequestMode):
    """

    volume_id: UUID
    mode: VmVolumeAttachmentRequestMode

    def to_dict(self) -> dict[str, Any]:
        volume_id = str(self.volume_id)

        mode: str = self.mode

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "volume_id": volume_id,
                "mode": mode,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        volume_id = UUID(d.pop("volume_id"))

        mode = check_vm_volume_attachment_request_mode(d.pop("mode"))

        vm_volume_attachment_request = cls(
            volume_id=volume_id,
            mode=mode,
        )

        return vm_volume_attachment_request
