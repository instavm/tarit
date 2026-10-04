from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar

from attrs import define as _attrs_define
from attrs import field as _attrs_field

T = TypeVar("T", bound="BalloonState")


@_attrs_define
class BalloonState:
    """
    Attributes:
        target_mib (int):
        actual_mib (int):
        target_pages (int):
        actual_pages (int):
    """

    target_mib: int
    actual_mib: int
    target_pages: int
    actual_pages: int
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        target_mib = self.target_mib

        actual_mib = self.actual_mib

        target_pages = self.target_pages

        actual_pages = self.actual_pages

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "target_mib": target_mib,
                "actual_mib": actual_mib,
                "target_pages": target_pages,
                "actual_pages": actual_pages,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        target_mib = d.pop("target_mib")

        actual_mib = d.pop("actual_mib")

        target_pages = d.pop("target_pages")

        actual_pages = d.pop("actual_pages")

        balloon_state = cls(
            target_mib=target_mib,
            actual_mib=actual_mib,
            target_pages=target_pages,
            actual_pages=actual_pages,
        )

        balloon_state.additional_properties = d
        return balloon_state

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
