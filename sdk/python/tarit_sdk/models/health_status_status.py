from typing import Literal

HealthStatusStatus = Literal["ok", "unhealthy"]

HEALTH_STATUS_STATUS_VALUES: set[HealthStatusStatus] = {
    "ok",
    "unhealthy",
}


def check_health_status_status(value: str) -> HealthStatusStatus:
    if value in HEALTH_STATUS_STATUS_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {HEALTH_STATUS_STATUS_VALUES!r}")
