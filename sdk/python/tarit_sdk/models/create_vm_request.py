from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define

from ..types import UNSET, Unset

if TYPE_CHECKING:
    from ..models.vm_volume_attachment_request import VmVolumeAttachmentRequest


T = TypeVar("T", bound="CreateVmRequest")


@_attrs_define
class CreateVmRequest:
    """All fields optional; defaults come from node configuration. Persistent block attachments are explicit, ordered after
    the root disk, exact-host constrained for local_block, and remain external across snapshot/hibernate operations.

        Attributes:
            id (UUID | Unset):
            memory_mib (int | Unset):  Default: 256.
            vcpus (int | Unset):  Default: 1.
            kernel_path (str | Unset):
            image (str | Unset):
            rootfs_path (str | Unset):
            cmdline (str | Unset):
            volumes (list[VmVolumeAttachmentRequest] | Unset):
    """

    id: UUID | Unset = UNSET
    memory_mib: int | Unset = 256
    vcpus: int | Unset = 1
    kernel_path: str | Unset = UNSET
    image: str | Unset = UNSET
    rootfs_path: str | Unset = UNSET
    cmdline: str | Unset = UNSET
    volumes: list[VmVolumeAttachmentRequest] | Unset = UNSET

    def to_dict(self) -> dict[str, Any]:
        id: str | Unset = UNSET
        if not isinstance(self.id, Unset):
            id = str(self.id)

        memory_mib = self.memory_mib

        vcpus = self.vcpus

        kernel_path = self.kernel_path

        image = self.image

        rootfs_path = self.rootfs_path

        cmdline = self.cmdline

        volumes: list[dict[str, Any]] | Unset = UNSET
        if not isinstance(self.volumes, Unset):
            volumes = []
            for volumes_item_data in self.volumes:
                volumes_item = volumes_item_data.to_dict()
                volumes.append(volumes_item)

        field_dict: dict[str, Any] = {}

        field_dict.update({})
        if id is not UNSET:
            field_dict["id"] = id
        if memory_mib is not UNSET:
            field_dict["memory_mib"] = memory_mib
        if vcpus is not UNSET:
            field_dict["vcpus"] = vcpus
        if kernel_path is not UNSET:
            field_dict["kernel_path"] = kernel_path
        if image is not UNSET:
            field_dict["image"] = image
        if rootfs_path is not UNSET:
            field_dict["rootfs_path"] = rootfs_path
        if cmdline is not UNSET:
            field_dict["cmdline"] = cmdline
        if volumes is not UNSET:
            field_dict["volumes"] = volumes

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.vm_volume_attachment_request import VmVolumeAttachmentRequest  # noqa: PLC0415

        d = dict(src_dict)
        _id = d.pop("id", UNSET)
        id: UUID | Unset
        if isinstance(_id, Unset):
            id = UNSET
        else:
            id = UUID(_id)

        memory_mib = d.pop("memory_mib", UNSET)

        vcpus = d.pop("vcpus", UNSET)

        kernel_path = d.pop("kernel_path", UNSET)

        image = d.pop("image", UNSET)

        rootfs_path = d.pop("rootfs_path", UNSET)

        cmdline = d.pop("cmdline", UNSET)

        _volumes = d.pop("volumes", UNSET)
        volumes: list[VmVolumeAttachmentRequest] | Unset = UNSET
        if _volumes is not UNSET:
            volumes = []
            for volumes_item_data in _volumes:
                volumes_item = VmVolumeAttachmentRequest.from_dict(volumes_item_data)

                volumes.append(volumes_item)

        create_vm_request = cls(
            id=id,
            memory_mib=memory_mib,
            vcpus=vcpus,
            kernel_path=kernel_path,
            image=image,
            rootfs_path=rootfs_path,
            cmdline=cmdline,
            volumes=volumes,
        )

        return create_vm_request
