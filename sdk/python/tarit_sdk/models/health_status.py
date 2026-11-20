from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, TypeVar

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..models.health_status_status import HealthStatusStatus, check_health_status_status
from ..types import UNSET, Unset

if TYPE_CHECKING:
    from ..models.health_status_checks import HealthStatusChecks


T = TypeVar("T", bound="HealthStatus")


@_attrs_define
class HealthStatus:
    """
    Attributes:
        status (HealthStatusStatus):
        checks (HealthStatusChecks | Unset): Named checks when provided. Servers running v0.1.0 return only status.
    """

    status: HealthStatusStatus
    checks: HealthStatusChecks | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        status: str = self.status

        checks: dict[str, Any] | Unset = UNSET
        if not isinstance(self.checks, Unset):
            checks = self.checks.to_dict()

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "status": status,
            }
        )
        if checks is not UNSET:
            field_dict["checks"] = checks

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.health_status_checks import HealthStatusChecks  # noqa: PLC0415

        d = dict(src_dict)
        status = check_health_status_status(d.pop("status"))

        _checks = d.pop("checks", UNSET)
        checks: HealthStatusChecks | Unset
        if isinstance(_checks, Unset):
            checks = UNSET
        else:
            checks = HealthStatusChecks.from_dict(_checks)

        health_status = cls(
            status=status,
            checks=checks,
        )

        health_status.additional_properties = d
        return health_status

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
