from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

T = TypeVar("T", bound="ExecuteRequest")


@_attrs_define
class ExecuteRequest:
    """
    Attributes:
        vm_id (UUID):
        command (str):
        timeout_ms (int | Unset):  Default: 30000.
    """

    vm_id: UUID
    command: str
    timeout_ms: int | Unset = 30000
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        vm_id = str(self.vm_id)

        command = self.command

        timeout_ms = self.timeout_ms

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "vm_id": vm_id,
                "command": command,
            }
        )
        if timeout_ms is not UNSET:
            field_dict["timeout_ms"] = timeout_ms

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        vm_id = UUID(d.pop("vm_id"))

        command = d.pop("command")

        timeout_ms = d.pop("timeout_ms", UNSET)

        execute_request = cls(
            vm_id=vm_id,
            command=command,
            timeout_ms=timeout_ms,
        )

        execute_request.additional_properties = d
        return execute_request

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
