from typing import Literal

VolumeProvider = Literal["local_block"]

VOLUME_PROVIDER_VALUES: set[VolumeProvider] = {
    "local_block",
}


def check_volume_provider(value: str) -> VolumeProvider:
    if value in VOLUME_PROVIDER_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {VOLUME_PROVIDER_VALUES!r}")
