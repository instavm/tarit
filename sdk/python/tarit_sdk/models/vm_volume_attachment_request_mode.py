from typing import Literal

VmVolumeAttachmentRequestMode = Literal["read_only", "read_write"]

VM_VOLUME_ATTACHMENT_REQUEST_MODE_VALUES: set[VmVolumeAttachmentRequestMode] = {
    "read_only",
    "read_write",
}


def check_vm_volume_attachment_request_mode(value: str) -> VmVolumeAttachmentRequestMode:
    if value in VM_VOLUME_ATTACHMENT_REQUEST_MODE_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {VM_VOLUME_ATTACHMENT_REQUEST_MODE_VALUES!r}")
