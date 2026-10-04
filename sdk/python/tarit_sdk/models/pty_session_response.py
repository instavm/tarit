from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import Any, TypeVar, cast
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

T = TypeVar("T", bound="PtySessionResponse")


@_attrs_define
class PtySessionResponse:
    """
    Attributes:
        pty_id (UUID):
        vm_id (UUID):
        cols (int):
        rows (int):
        created_at (datetime.datetime):
        shell (None | str | Unset):
    """

    pty_id: UUID
    vm_id: UUID
    cols: int
    rows: int
    created_at: datetime.datetime
    shell: None | str | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        pty_id = str(self.pty_id)

        vm_id = str(self.vm_id)

        cols = self.cols

        rows = self.rows

        created_at = self.created_at.isoformat()

        shell: None | str | Unset
        if isinstance(self.shell, Unset):
            shell = UNSET
        else:
            shell = self.shell

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "pty_id": pty_id,
                "vm_id": vm_id,
                "cols": cols,
                "rows": rows,
                "created_at": created_at,
            }
        )
        if shell is not UNSET:
            field_dict["shell"] = shell

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        pty_id = UUID(d.pop("pty_id"))

        vm_id = UUID(d.pop("vm_id"))

        cols = d.pop("cols")

        rows = d.pop("rows")

        created_at = datetime.datetime.fromisoformat(d.pop("created_at"))

        def _parse_shell(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        shell = _parse_shell(d.pop("shell", UNSET))

        pty_session_response = cls(
            pty_id=pty_id,
            vm_id=vm_id,
            cols=cols,
            rows=rows,
            created_at=created_at,
            shell=shell,
        )

        pty_session_response.additional_properties = d
        return pty_session_response

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
