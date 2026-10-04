from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, TypeVar

from attrs import define as _attrs_define
from attrs import field as _attrs_field

if TYPE_CHECKING:
    from ..models.get_cluster_response_200_nodes_item import GetClusterResponse200NodesItem


T = TypeVar("T", bound="GetClusterResponse200")


@_attrs_define
class GetClusterResponse200:
    """
    Attributes:
        this_host (str):
        clustered (bool):
        total_nodes (int):
        healthy_nodes (int): Nodes marked healthy with a heartbeat in the last 15 seconds.
        cluster_free_vcpus (int): Sum of free vcpus across healthy nodes.
        cluster_free_memory_mib (int): Sum of free memory across healthy nodes.
        nodes (list[GetClusterResponse200NodesItem]):
    """

    this_host: str
    clustered: bool
    total_nodes: int
    healthy_nodes: int
    cluster_free_vcpus: int
    cluster_free_memory_mib: int
    nodes: list[GetClusterResponse200NodesItem]
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        this_host = self.this_host

        clustered = self.clustered

        total_nodes = self.total_nodes

        healthy_nodes = self.healthy_nodes

        cluster_free_vcpus = self.cluster_free_vcpus

        cluster_free_memory_mib = self.cluster_free_memory_mib

        nodes = []
        for nodes_item_data in self.nodes:
            nodes_item = nodes_item_data.to_dict()
            nodes.append(nodes_item)

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "this_host": this_host,
                "clustered": clustered,
                "total_nodes": total_nodes,
                "healthy_nodes": healthy_nodes,
                "cluster_free_vcpus": cluster_free_vcpus,
                "cluster_free_memory_mib": cluster_free_memory_mib,
                "nodes": nodes,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.get_cluster_response_200_nodes_item import GetClusterResponse200NodesItem  # noqa: PLC0415

        d = dict(src_dict)
        this_host = d.pop("this_host")

        clustered = d.pop("clustered")

        total_nodes = d.pop("total_nodes")

        healthy_nodes = d.pop("healthy_nodes")

        cluster_free_vcpus = d.pop("cluster_free_vcpus")

        cluster_free_memory_mib = d.pop("cluster_free_memory_mib")

        nodes = []
        _nodes = d.pop("nodes")
        for nodes_item_data in _nodes:
            nodes_item = GetClusterResponse200NodesItem.from_dict(nodes_item_data)

            nodes.append(nodes_item)

        get_cluster_response_200 = cls(
            this_host=this_host,
            clustered=clustered,
            total_nodes=total_nodes,
            healthy_nodes=healthy_nodes,
            cluster_free_vcpus=cluster_free_vcpus,
            cluster_free_memory_mib=cluster_free_memory_mib,
            nodes=nodes,
        )

        get_cluster_response_200.additional_properties = d
        return get_cluster_response_200

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
