from typing import Literal

VmRecordStartupPath = Literal["cold", "snapshot_restore", "warm"]

VM_RECORD_STARTUP_PATH_VALUES: set[VmRecordStartupPath] = {
    "cold",
    "snapshot_restore",
    "warm",
}


def check_vm_record_startup_path(value: str) -> VmRecordStartupPath:
    if value in VM_RECORD_STARTUP_PATH_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {VM_RECORD_STARTUP_PATH_VALUES!r}")
