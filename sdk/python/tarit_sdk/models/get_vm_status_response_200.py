from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..models.get_vm_status_response_200_state import (
    GetVmStatusResponse200State,
    check_get_vm_status_response_200_state,
)

T = TypeVar("T", bound="GetVmStatusResponse200")


@_attrs_define
class GetVmStatusResponse200:
    """
    Attributes:
        state (GetVmStatusResponse200State):
        uptime_ms (int):
        vcpus (int):
        mem_mib (int):
        vcpu_alive (bool):
    """

    state: GetVmStatusResponse200State
    uptime_ms: int
    vcpus: int
    mem_mib: int
    vcpu_alive: bool
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        state: str = self.state

        uptime_ms = self.uptime_ms

        vcpus = self.vcpus

        mem_mib = self.mem_mib

        vcpu_alive = self.vcpu_alive

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "state": state,
                "uptime_ms": uptime_ms,
                "vcpus": vcpus,
                "mem_mib": mem_mib,
                "vcpu_alive": vcpu_alive,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        state = check_get_vm_status_response_200_state(d.pop("state"))

        uptime_ms = d.pop("uptime_ms")

        vcpus = d.pop("vcpus")

        mem_mib = d.pop("mem_mib")

        vcpu_alive = d.pop("vcpu_alive")

        get_vm_status_response_200 = cls(
            state=state,
            uptime_ms=uptime_ms,
            vcpus=vcpus,
            mem_mib=mem_mib,
            vcpu_alive=vcpu_alive,
        )

        get_vm_status_response_200.additional_properties = d
        return get_vm_status_response_200

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
