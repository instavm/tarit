from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, TypeVar

from attrs import define as _attrs_define
from attrs import field as _attrs_field

if TYPE_CHECKING:
    from ..models.warm_pool_status_classes_item import WarmPoolStatusClassesItem
    from ..models.warm_pool_status_disk_pressure import WarmPoolStatusDiskPressure
    from ..models.warm_pool_status_learn import WarmPoolStatusLearn


T = TypeVar("T", bound="WarmPoolStatus")


@_attrs_define
class WarmPoolStatus:
    """
    Attributes:
        enabled (bool):
        cpu_overcommit (float):
        replenish_concurrency (int):
        total_target (int):
        learn (WarmPoolStatusLearn): Learning limits, target depth, and idle timeout.
        disk_pressure (WarmPoolStatusDiskPressure): Node-local disk pressure, usage, reservation, and cleanup counters.
        classes (list[WarmPoolStatusClassesItem]): Configured and learned warm classes and their current depth.
    """

    enabled: bool
    cpu_overcommit: float
    replenish_concurrency: int
    total_target: int
    learn: WarmPoolStatusLearn
    disk_pressure: WarmPoolStatusDiskPressure
    classes: list[WarmPoolStatusClassesItem]
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        enabled = self.enabled

        cpu_overcommit = self.cpu_overcommit

        replenish_concurrency = self.replenish_concurrency

        total_target = self.total_target

        learn = self.learn.to_dict()

        disk_pressure = self.disk_pressure.to_dict()

        classes = []
        for classes_item_data in self.classes:
            classes_item = classes_item_data.to_dict()
            classes.append(classes_item)

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "enabled": enabled,
                "cpu_overcommit": cpu_overcommit,
                "replenish_concurrency": replenish_concurrency,
                "total_target": total_target,
                "learn": learn,
                "disk_pressure": disk_pressure,
                "classes": classes,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.warm_pool_status_classes_item import WarmPoolStatusClassesItem  # noqa: PLC0415
        from ..models.warm_pool_status_disk_pressure import WarmPoolStatusDiskPressure  # noqa: PLC0415
        from ..models.warm_pool_status_learn import WarmPoolStatusLearn  # noqa: PLC0415

        d = dict(src_dict)
        enabled = d.pop("enabled")

        cpu_overcommit = d.pop("cpu_overcommit")

        replenish_concurrency = d.pop("replenish_concurrency")

        total_target = d.pop("total_target")

        learn = WarmPoolStatusLearn.from_dict(d.pop("learn"))

        disk_pressure = WarmPoolStatusDiskPressure.from_dict(d.pop("disk_pressure"))

        classes = []
        _classes = d.pop("classes")
        for classes_item_data in _classes:
            classes_item = WarmPoolStatusClassesItem.from_dict(classes_item_data)

            classes.append(classes_item)

        warm_pool_status = cls(
            enabled=enabled,
            cpu_overcommit=cpu_overcommit,
            replenish_concurrency=replenish_concurrency,
            total_target=total_target,
            learn=learn,
            disk_pressure=disk_pressure,
            classes=classes,
        )

        warm_pool_status.additional_properties = d
        return warm_pool_status

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
