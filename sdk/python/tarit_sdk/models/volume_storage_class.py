from typing import Literal

VolumeStorageClass = Literal["block", "filesystem", "object"]

VOLUME_STORAGE_CLASS_VALUES: set[VolumeStorageClass] = {
    "block",
    "filesystem",
    "object",
}


def check_volume_storage_class(value: str) -> VolumeStorageClass:
    if value in VOLUME_STORAGE_CLASS_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {VOLUME_STORAGE_CLASS_VALUES!r}")
