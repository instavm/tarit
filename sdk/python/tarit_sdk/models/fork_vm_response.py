from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

if TYPE_CHECKING:
    from ..models.fork_metrics import ForkMetrics
    from ..models.vm_record import VmRecord


T = TypeVar("T", bound="ForkVmResponse")


@_attrs_define
class ForkVmResponse:
    """
    Attributes:
        source_vm_id (UUID):
        vm (VmRecord): Tenant-safe VM record. Host identity, filesystem paths, VMM socket, process id, boot arguments,
            and ownership metadata are never exposed.
        metrics (ForkMetrics | Unset):
    """

    source_vm_id: UUID
    vm: VmRecord
    metrics: ForkMetrics | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        source_vm_id = str(self.source_vm_id)

        vm = self.vm.to_dict()

        metrics: dict[str, Any] | Unset = UNSET
        if not isinstance(self.metrics, Unset):
            metrics = self.metrics.to_dict()

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "source_vm_id": source_vm_id,
                "vm": vm,
            }
        )
        if metrics is not UNSET:
            field_dict["metrics"] = metrics

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.fork_metrics import ForkMetrics  # noqa: PLC0415
        from ..models.vm_record import VmRecord  # noqa: PLC0415

        d = dict(src_dict)
        source_vm_id = UUID(d.pop("source_vm_id"))

        vm = VmRecord.from_dict(d.pop("vm"))

        _metrics = d.pop("metrics", UNSET)
        metrics: ForkMetrics | Unset
        if isinstance(_metrics, Unset):
            metrics = UNSET
        else:
            metrics = ForkMetrics.from_dict(_metrics)

        fork_vm_response = cls(
            source_vm_id=source_vm_id,
            vm=vm,
            metrics=metrics,
        )

        fork_vm_response.additional_properties = d
        return fork_vm_response

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
