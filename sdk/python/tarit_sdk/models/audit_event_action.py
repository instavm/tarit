from typing import Literal

AuditEventAction = Literal[
    "attach_pty",
    "create",
    "create_share",
    "create_volume",
    "delete",
    "delete_volume",
    "exec",
    "issue_share_token",
    "pause",
    "restore",
    "resume",
    "revoke_share",
    "snapshot",
    "ssh_attempt",
    "suspend",
    "update_egress",
    "update_share",
]

AUDIT_EVENT_ACTION_VALUES: set[AuditEventAction] = {
    "attach_pty",
    "create",
    "create_share",
    "create_volume",
    "delete",
    "delete_volume",
    "exec",
    "issue_share_token",
    "pause",
    "restore",
    "resume",
    "revoke_share",
    "snapshot",
    "ssh_attempt",
    "suspend",
    "update_egress",
    "update_share",
}


def check_audit_event_action(value: str) -> AuditEventAction:
    if value in AUDIT_EVENT_ACTION_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {AUDIT_EVENT_ACTION_VALUES!r}")
