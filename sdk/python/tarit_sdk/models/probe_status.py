from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, TypeVar

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..models.probe_status_status import ProbeStatusStatus, check_probe_status_status

if TYPE_CHECKING:
    from ..models.probe_status_checks import ProbeStatusChecks


T = TypeVar("T", bound="ProbeStatus")


@_attrs_define
class ProbeStatus:
    """
    Attributes:
        status (ProbeStatusStatus):
        checks (ProbeStatusChecks): Named checks and their state. Check names vary by probe and configuration.
    """

    status: ProbeStatusStatus
    checks: ProbeStatusChecks
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        status: str = self.status

        checks = self.checks.to_dict()

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "status": status,
                "checks": checks,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.probe_status_checks import ProbeStatusChecks  # noqa: PLC0415

        d = dict(src_dict)
        status = check_probe_status_status(d.pop("status"))

        checks = ProbeStatusChecks.from_dict(d.pop("checks"))

        probe_status = cls(
            status=status,
            checks=checks,
        )

        probe_status.additional_properties = d
        return probe_status

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
