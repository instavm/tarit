from typing import Literal

CreateVolumeRequestProvider = Literal["local_block", "nfs_v4_1_block"]

CREATE_VOLUME_REQUEST_PROVIDER_VALUES: set[CreateVolumeRequestProvider] = {
    "local_block",
    "nfs_v4_1_block",
}


def check_create_volume_request_provider(value: str) -> CreateVolumeRequestProvider:
    if value in CREATE_VOLUME_REQUEST_PROVIDER_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {CREATE_VOLUME_REQUEST_PROVIDER_VALUES!r}")
