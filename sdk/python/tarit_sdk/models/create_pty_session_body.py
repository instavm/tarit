from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

T = TypeVar("T", bound="CreatePtySessionBody")


@_attrs_define
class CreatePtySessionBody:
    """
    Attributes:
        cols (int):
        rows (int):
        shell (str | Unset): Shell to launch in the guest; the guest agent default is used when omitted.
    """

    cols: int
    rows: int
    shell: str | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        cols = self.cols

        rows = self.rows

        shell = self.shell

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "cols": cols,
                "rows": rows,
            }
        )
        if shell is not UNSET:
            field_dict["shell"] = shell

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        cols = d.pop("cols")

        rows = d.pop("rows")

        shell = d.pop("shell", UNSET)

        create_pty_session_body = cls(
            cols=cols,
            rows=rows,
            shell=shell,
        )

        create_pty_session_body.additional_properties = d
        return create_pty_session_body

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
