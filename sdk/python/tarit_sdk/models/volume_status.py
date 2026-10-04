from typing import Literal

VolumeStatus = Literal["available", "creating", "deleting", "error"]

VOLUME_STATUS_VALUES: set[VolumeStatus] = {
    "available",
    "creating",
    "deleting",
    "error",
}


def check_volume_status(value: str) -> VolumeStatus:
    if value in VOLUME_STATUS_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {VOLUME_STATUS_VALUES!r}")
