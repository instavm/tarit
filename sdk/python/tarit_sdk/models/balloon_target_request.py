from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar

from attrs import define as _attrs_define

T = TypeVar("T", bound="BalloonTargetRequest")


@_attrs_define
class BalloonTargetRequest:
    """
    Attributes:
        target_mib (int): Requested guest memory to return to the host; the VMM rejects targets larger than guest RAM.
    """

    target_mib: int

    def to_dict(self) -> dict[str, Any]:
        target_mib = self.target_mib

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "target_mib": target_mib,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        target_mib = d.pop("target_mib")

        balloon_target_request = cls(
            target_mib=target_mib,
        )

        return balloon_target_request
