from typing import Literal

ExecutionRecordStatus = Literal["completed", "failed", "pending", "running"]

EXECUTION_RECORD_STATUS_VALUES: set[ExecutionRecordStatus] = {
    "completed",
    "failed",
    "pending",
    "running",
}


def check_execution_record_status(value: str) -> ExecutionRecordStatus:
    if value in EXECUTION_RECORD_STATUS_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {EXECUTION_RECORD_STATUS_VALUES!r}")
