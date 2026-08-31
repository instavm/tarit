from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar

from attrs import define as _attrs_define
from attrs import field as _attrs_field

T = TypeVar("T", bound="UsageSummary")


@_attrs_define
class UsageSummary:
    """
    Attributes:
        api_key_id (str):
        owner_key (str):
        vm_runtime_seconds (float):
        exec_count (int):
        exec_duration_ms (int):
    """

    api_key_id: str
    owner_key: str
    vm_runtime_seconds: float
    exec_count: int
    exec_duration_ms: int
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        api_key_id = self.api_key_id

        owner_key = self.owner_key

        vm_runtime_seconds = self.vm_runtime_seconds

        exec_count = self.exec_count

        exec_duration_ms = self.exec_duration_ms

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "api_key_id": api_key_id,
                "owner_key": owner_key,
                "vm_runtime_seconds": vm_runtime_seconds,
                "exec_count": exec_count,
                "exec_duration_ms": exec_duration_ms,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        api_key_id = d.pop("api_key_id")

        owner_key = d.pop("owner_key")

        vm_runtime_seconds = d.pop("vm_runtime_seconds")

        exec_count = d.pop("exec_count")

        exec_duration_ms = d.pop("exec_duration_ms")

        usage_summary = cls(
            api_key_id=api_key_id,
            owner_key=owner_key,
            vm_runtime_seconds=vm_runtime_seconds,
            exec_count=exec_count,
            exec_duration_ms=exec_duration_ms,
        )

        usage_summary.additional_properties = d
        return usage_summary

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
