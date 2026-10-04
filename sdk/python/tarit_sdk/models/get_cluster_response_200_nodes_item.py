from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import Any, TypeVar

from attrs import define as _attrs_define
from attrs import field as _attrs_field

T = TypeVar("T", bound="GetClusterResponse200NodesItem")


@_attrs_define
class GetClusterResponse200NodesItem:
    """
    Attributes:
        host_id (str):
        rpc_addr (str):
        sandbox_count (int):
        free_vcpus (int):
        free_memory_mib (int):
        up (bool):
        last_heartbeat (datetime.datetime):
    """

    host_id: str
    rpc_addr: str
    sandbox_count: int
    free_vcpus: int
    free_memory_mib: int
    up: bool
    last_heartbeat: datetime.datetime
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        host_id = self.host_id

        rpc_addr = self.rpc_addr

        sandbox_count = self.sandbox_count

        free_vcpus = self.free_vcpus

        free_memory_mib = self.free_memory_mib

        up = self.up

        last_heartbeat = self.last_heartbeat.isoformat()

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "host_id": host_id,
                "rpc_addr": rpc_addr,
                "sandbox_count": sandbox_count,
                "free_vcpus": free_vcpus,
                "free_memory_mib": free_memory_mib,
                "up": up,
                "last_heartbeat": last_heartbeat,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        host_id = d.pop("host_id")

        rpc_addr = d.pop("rpc_addr")

        sandbox_count = d.pop("sandbox_count")

        free_vcpus = d.pop("free_vcpus")

        free_memory_mib = d.pop("free_memory_mib")

        up = d.pop("up")

        last_heartbeat = datetime.datetime.fromisoformat(d.pop("last_heartbeat"))

        get_cluster_response_200_nodes_item = cls(
            host_id=host_id,
            rpc_addr=rpc_addr,
            sandbox_count=sandbox_count,
            free_vcpus=free_vcpus,
            free_memory_mib=free_memory_mib,
            up=up,
            last_heartbeat=last_heartbeat,
        )

        get_cluster_response_200_nodes_item.additional_properties = d
        return get_cluster_response_200_nodes_item

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
