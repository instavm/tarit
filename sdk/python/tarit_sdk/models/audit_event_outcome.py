from typing import Literal

AuditEventOutcome = Literal["attempt", "denied", "error", "ok"]

AUDIT_EVENT_OUTCOME_VALUES: set[AuditEventOutcome] = {
    "attempt",
    "denied",
    "error",
    "ok",
}


def check_audit_event_outcome(value: str) -> AuditEventOutcome:
    if value in AUDIT_EVENT_OUTCOME_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {AUDIT_EVENT_OUTCOME_VALUES!r}")
