from typing import Literal

ProbeStatusStatus = Literal["ok", "unhealthy"]

PROBE_STATUS_STATUS_VALUES: set[ProbeStatusStatus] = {
    "ok",
    "unhealthy",
}


def check_probe_status_status(value: str) -> ProbeStatusStatus:
    if value in PROBE_STATUS_STATUS_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {PROBE_STATUS_STATUS_VALUES!r}")
