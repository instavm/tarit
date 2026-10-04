from typing import Literal

VmRecordStatus = Literal["creating", "error", "hibernated", "paused", "running", "stopped", "suspended"]

VM_RECORD_STATUS_VALUES: set[VmRecordStatus] = {
    "creating",
    "error",
    "hibernated",
    "paused",
    "running",
    "stopped",
    "suspended",
}


def check_vm_record_status(value: str) -> VmRecordStatus:
    if value in VM_RECORD_STATUS_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {VM_RECORD_STATUS_VALUES!r}")
