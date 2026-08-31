from typing import Literal

GetVmStatusResponse200State = Literal["created", "paused", "running", "stopped", "suspended"]

GET_VM_STATUS_RESPONSE_200_STATE_VALUES: set[GetVmStatusResponse200State] = {
    "created",
    "paused",
    "running",
    "stopped",
    "suspended",
}


def check_get_vm_status_response_200_state(value: str) -> GetVmStatusResponse200State:
    if value in GET_VM_STATUS_RESPONSE_200_STATE_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {GET_VM_STATUS_RESPONSE_200_STATE_VALUES!r}")
