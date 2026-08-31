from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import Any, TypeVar, cast
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..models.audit_event_action import AuditEventAction, check_audit_event_action
from ..models.audit_event_outcome import AuditEventOutcome, check_audit_event_outcome
from ..types import UNSET, Unset

T = TypeVar("T", bound="AuditEvent")


@_attrs_define
class AuditEvent:
    """
    Attributes:
        id (UUID):
        api_key_id (str):
        owner_key (str):
        host_id (str):
        action (AuditEventAction):
        outcome (AuditEventOutcome):
        created_at (datetime.datetime):
        vm_id (None | Unset | UUID):
        detail (None | str | Unset):
    """

    id: UUID
    api_key_id: str
    owner_key: str
    host_id: str
    action: AuditEventAction
    outcome: AuditEventOutcome
    created_at: datetime.datetime
    vm_id: None | Unset | UUID = UNSET
    detail: None | str | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        id = str(self.id)

        api_key_id = self.api_key_id

        owner_key = self.owner_key

        host_id = self.host_id

        action: str = self.action

        outcome: str = self.outcome

        created_at = self.created_at.isoformat()

        vm_id: None | str | Unset
        if isinstance(self.vm_id, Unset):
            vm_id = UNSET
        elif isinstance(self.vm_id, UUID):
            vm_id = str(self.vm_id)
        else:
            vm_id = self.vm_id

        detail: None | str | Unset
        if isinstance(self.detail, Unset):
            detail = UNSET
        else:
            detail = self.detail

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "id": id,
                "api_key_id": api_key_id,
                "owner_key": owner_key,
                "host_id": host_id,
                "action": action,
                "outcome": outcome,
                "created_at": created_at,
            }
        )
        if vm_id is not UNSET:
            field_dict["vm_id"] = vm_id
        if detail is not UNSET:
            field_dict["detail"] = detail

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        id = UUID(d.pop("id"))

        api_key_id = d.pop("api_key_id")

        owner_key = d.pop("owner_key")

        host_id = d.pop("host_id")

        action = check_audit_event_action(d.pop("action"))

        outcome = check_audit_event_outcome(d.pop("outcome"))

        created_at = datetime.datetime.fromisoformat(d.pop("created_at"))

        def _parse_vm_id(data: object) -> None | Unset | UUID:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            try:
                if not isinstance(data, str):
                    raise TypeError()
                vm_id_type_0 = UUID(data)

                return vm_id_type_0
            except (TypeError, ValueError, AttributeError, KeyError):
                pass
            return cast(None | Unset | UUID, data)

        vm_id = _parse_vm_id(d.pop("vm_id", UNSET))

        def _parse_detail(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        detail = _parse_detail(d.pop("detail", UNSET))

        audit_event = cls(
            id=id,
            api_key_id=api_key_id,
            owner_key=owner_key,
            host_id=host_id,
            action=action,
            outcome=outcome,
            created_at=created_at,
            vm_id=vm_id,
            detail=detail,
        )

        audit_event.additional_properties = d
        return audit_event

    @property
    def additional_keys(self) -> list[str]:
        return list(self.additional_properties.keys())

    def __getitem__(self, key: str) -> Any:
        return self.additional_properties[key]

    def __setitem__(self, key: str, value: Any) -> None:
        self.additional_properties[key] = value

    def __delitem__(self, key: str) -> None:
        del self.additional_properties[key]

    def __contains__(self, key: str) -> bool:
        return key in self.additional_properties
