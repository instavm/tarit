from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar

from attrs import define as _attrs_define

T = TypeVar("T", bound="VolumeCapabilities")


@_attrs_define
class VolumeCapabilities:
    """
    Attributes:
        read_only_many (bool):
        read_write_once (bool):
        read_write_many (bool):
        snapshots (bool):
        clones (bool):
    """

    read_only_many: bool
    read_write_once: bool
    read_write_many: bool
    snapshots: bool
    clones: bool

    def to_dict(self) -> dict[str, Any]:
        read_only_many = self.read_only_many

        read_write_once = self.read_write_once

        read_write_many = self.read_write_many

        snapshots = self.snapshots

        clones = self.clones

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "read_only_many": read_only_many,
                "read_write_once": read_write_once,
                "read_write_many": read_write_many,
                "snapshots": snapshots,
                "clones": clones,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        read_only_many = d.pop("read_only_many")

        read_write_once = d.pop("read_write_once")

        read_write_many = d.pop("read_write_many")

        snapshots = d.pop("snapshots")

        clones = d.pop("clones")

        volume_capabilities = cls(
            read_only_many=read_only_many,
            read_write_once=read_write_once,
            read_write_many=read_write_many,
            snapshots=snapshots,
            clones=clones,
        )

        return volume_capabilities
